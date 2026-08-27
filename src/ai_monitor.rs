//! Read-only Windows AI work monitor.
//!
//! A lightweight sampler records local Codex/process counters once per minute
//! so the trend survives while the Widget board is closed. Network route and
//! latency probes remain on-demand and separately cached. No credentials,
//! command lines, message bodies, or file contents are returned.

use crate::platform::NoWindow;
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const NETWORK_TTL: Duration = Duration::from_secs(60);
const POWER_TTL: Duration = Duration::from_secs(60);
const AI_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);
const AI_SAMPLE_MIN_AGE: Duration = Duration::from_secs(55);
const AI_WORK_TTL: Duration = Duration::from_secs(75);
const AI_SAMPLE_GAP_SECS: u64 = 90;
const CODEX_TASK_LIMIT: usize = 8;
const TOKEN_HISTORY_LIMIT: usize = 60;
const TOKEN_HISTORY_WINDOW_MS: i64 = 60 * 60 * 1_000;
const TOKEN_TREND_SCHEMA_VERSION: u32 = 1;
const TOKEN_TREND_FILE: &str = "ai_monitor_token_trend.jsonl";
const ACCEPTANCE_EVENT_KINDS: [&str; 9] = [
    "TOKEN_ACTIVE",
    "TOKEN_IDLE",
    "APPS_CLOSED",
    "SOURCE_MISSING",
    "SAMPLING_GAP",
    "WINDOWS_LOCKED",
    "WINDOWS_UNLOCKED",
    "HISTORY_RESTORED",
    "STANDBY_CYCLE",
];
const SPEED_TEST_BYTES: u64 = 5_000_000;
const SPEED_TEST_URL: &str = "https://speed.cloudflare.com/__down?bytes=5000000";

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceLevel {
    Measured,
    Inferred,
    #[default]
    MissingProof,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AiMonitorSnapshot {
    pub version: String,
    pub captured_at: String,
    pub collection_ms: u64,
    pub cache_ttl_secs: u64,
    pub network: NetworkSnapshot,
    pub ai_work: AiWorkSnapshot,
    pub codex_health: CodexHealthView,
    pub power: PowerSnapshot,
    pub recovery: RecoverySnapshot,
    pub acceptance: MonitorAcceptanceView,
    pub overhead: MonitorOverheadSnapshot,
    pub alerts: Vec<LocalAlertView>,
    pub safety: SafetyView,
}

#[derive(Debug, Clone, Serialize)]
pub struct SafetyView {
    pub local_only: bool,
    pub read_only: bool,
    pub credentials_accessed: bool,
    pub background_sampling: bool,
    pub automatic_download_test: bool,
    pub execution_request_managed: bool,
}

impl Default for SafetyView {
    fn default() -> Self {
        Self {
            local_only: true,
            read_only: true,
            credentials_accessed: false,
            background_sampling: BACKGROUND_SAMPLER_RUNNING.load(Ordering::Relaxed),
            automatic_download_test: false,
            execution_request_managed: awake_guard_enabled(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PowerSnapshot {
    pub evidence: EvidenceLevel,
    pub session_state: String,
    pub session_locked: Option<bool>,
    pub session_evidence: EvidenceLevel,
    pub session_source_error: Option<String>,
    pub awake_guard: AwakeGuardView,
    pub modern_standby: ModernStandbyView,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AwakeGuardView {
    pub enabled: bool,
    pub chatgpt_running: bool,
    pub request_active: bool,
    pub system_required: bool,
    pub display_required: bool,
    pub last_updated_at_ms: Option<i64>,
    pub evidence: EvidenceLevel,
    pub source_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModernStandbyView {
    pub capable: Option<bool>,
    pub last_entered_at: Option<String>,
    pub last_entered_at_ms: Option<i64>,
    pub last_resumed_at: Option<String>,
    pub last_resumed_at_ms: Option<i64>,
    pub entered_since_monitor_start: bool,
    pub evidence: EvidenceLevel,
    pub source_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RecoverySnapshot {
    pub status: String,
    pub monitor_started_at_ms: i64,
    pub uptime_secs: u64,
    pub last_ai_sample_at_ms: Option<i64>,
    pub sample_age_secs: Option<u64>,
    pub token_history_restored: bool,
    pub token_history_persisted: bool,
    pub evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MonitorAcceptanceView {
    pub status: String,
    pub evidence: EvidenceLevel,
    pub modes: Vec<AcceptanceModeView>,
    pub missing_required_modes: Vec<String>,
    pub ledger_persisted: bool,
    pub ledger_restored: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AcceptanceModeView {
    pub mode: String,
    pub required: bool,
    pub status: String,
    pub last_observed_at_ms: Option<i64>,
    pub observations: u64,
    pub evidence: EvidenceLevel,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AcceptanceEventRecord {
    first_observed_at_ms: i64,
    last_observed_at_ms: i64,
    observations: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MonitorOverheadSnapshot {
    pub evidence: EvidenceLevel,
    pub scope: String,
    pub sampler_interval_secs: u64,
    pub network_cache_ttl_secs: u64,
    pub power_cache_ttl_secs: u64,
    pub ai_cache_ttl_secs: u64,
    pub sampler_runs_total: u64,
    pub last_sampler_wall_ms: f64,
    pub last_sampler_cpu_ms: Option<f64>,
    pub ai_collections_total: u64,
    pub ai_cache_hits_total: u64,
    pub last_ai_collection_ms: f64,
    pub network_collections_total: u64,
    pub network_cache_hits_total: u64,
    pub last_network_collection_ms: f64,
    pub standby_collections_total: u64,
    pub standby_cache_hits_total: u64,
    pub last_standby_collection_ms: f64,
    pub gap_power_event_queries_total: u64,
    pub trend_writes_total: u64,
    pub trend_file_bytes: Option<u64>,
    pub manual_speed_tests_total: u64,
    pub manual_download_bytes_total: u64,
    pub background_network_probes: bool,
    pub background_download_tests: bool,
    pub process: SirinProcessResourceView,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SirinProcessResourceView {
    pub scope: String,
    pub cpu_percent_recent: Option<f64>,
    pub cpu_interval_secs: Option<f64>,
    pub cpu_evidence: EvidenceLevel,
    pub working_set_mb: Option<f64>,
    pub private_mb: Option<f64>,
    pub memory_evidence: EvidenceLevel,
    pub source_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LocalAlertView {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub action: String,
    pub evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkSnapshot {
    pub evidence: EvidenceLevel,
    pub default_routes: Vec<DefaultRouteView>,
    pub interfaces: Vec<InterfaceView>,
    pub wifi: Option<WifiView>,
    pub split_default_route: bool,
    pub warnings: Vec<String>,
    pub source_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DefaultRouteView {
    pub family: String,
    pub interface_index: u32,
    pub interface_alias: String,
    pub gateway: String,
    pub route_metric: u32,
    pub interface_metric: u32,
    pub effective_metric: u32,
    pub selected: bool,
    pub latency_target: Option<String>,
    pub latency_ms: Option<u32>,
    pub latency_status: String,
    pub latency_evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct InterfaceView {
    pub family: String,
    pub index: u32,
    pub alias: String,
    pub state: String,
    pub metric: u32,
    pub active_ipv4_default: bool,
    pub active_ipv6_default: bool,
    pub evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WifiView {
    pub state: Option<String>,
    pub ssid: Option<String>,
    pub signal_percent: Option<u32>,
    pub rssi_dbm_approx: Option<i32>,
    pub rssi_evidence: EvidenceLevel,
    pub receive_rate_mbps: Option<f64>,
    pub transmit_rate_mbps: Option<f64>,
    pub radio_type: Option<String>,
    pub channel: Option<u32>,
    pub evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AiWorkSnapshot {
    pub evidence: EvidenceLevel,
    pub processes: Vec<ProcessGroupView>,
    pub codex_tasks: Vec<CodexTaskView>,
    pub codex_recent_total_tokens: u64,
    pub codex_token_trend: TokenTrendView,
    pub codex_source: String,
    pub codex_source_error: Option<String>,
    pub process_source_error: Option<String>,
    pub system_resources: SystemResourceView,
    pub sirin_tokens: SirinTokenView,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CodexHealthView {
    pub status: String,
    pub severity: String,
    pub label: String,
    pub detail: String,
    pub evidence: EvidenceLevel,
    pub progress_status: String,
    pub progress_age_secs: Option<u64>,
    pub active_task_count: u32,
    pub token_delta: u64,
    pub local_resource_status: String,
    pub resource_summary: String,
    pub network_status: String,
    pub network_summary: String,
    pub remote_limit_status: String,
    pub remote_limit_evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SystemResourceView {
    pub evidence: EvidenceLevel,
    pub cpu_percent_recent: Option<f64>,
    pub cpu_interval_secs: Option<f64>,
    pub cpu_evidence: EvidenceLevel,
    pub physical_memory_total_gb: Option<f64>,
    pub physical_memory_available_gb: Option<f64>,
    pub physical_memory_available_percent: Option<f64>,
    pub committed_percent: Option<f64>,
    pub memory_evidence: EvidenceLevel,
    pub system_drive: Option<String>,
    pub system_drive_free_gb: Option<f64>,
    pub system_drive_free_percent: Option<f64>,
    pub disk_evidence: EvidenceLevel,
    pub ai_working_set_mb: f64,
    pub pressure: String,
    pub pressure_reasons: Vec<String>,
    pub source_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ProcessGroupView {
    pub app: String,
    pub running: bool,
    pub process_count: u32,
    pub working_set_mb: f64,
    pub evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CodexTaskView {
    pub id: String,
    pub display_name: String,
    pub tokens_used: u64,
    pub updated_at_ms: i64,
    pub activity: String,
    pub token_evidence: EvidenceLevel,
    pub activity_evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenTrendView {
    pub interval_secs: u64,
    pub delta_tokens: u64,
    pub tokens_per_min: u64,
    pub gap: bool,
    pub evidence: EvidenceLevel,
    pub lifecycle: String,
    pub source_available: bool,
    pub chatgpt_running: bool,
    pub codex_running: bool,
    pub history: Vec<TokenTrendPointView>,
    pub history_persisted: bool,
    pub history_restored: bool,
    pub persistence_error: Option<String>,
    pub last_sampled_at_ms: Option<i64>,
    pub last_attempted_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenTrendPointView {
    pub sampled_at_ms: i64,
    pub interval_secs: u64,
    pub delta_tokens: u64,
    pub tokens_per_min: u64,
    pub gap: bool,
    #[serde(default)]
    pub lifecycle: String,
    #[serde(default = "default_true")]
    pub source_available: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SirinTokenView {
    pub window_secs: u64,
    pub api_calls: u64,
    pub total_tokens: u64,
    pub tokens_per_min: u64,
    pub cache_hit_pct: f64,
    pub estimated_cost_per_hour_usd: f64,
    pub evidence: EvidenceLevel,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeedTestView {
    pub captured_at: String,
    pub endpoint: String,
    pub bytes_downloaded: u64,
    pub elapsed_ms: u64,
    pub megabits_per_second: f64,
    pub manual_only: bool,
    pub evidence: EvidenceLevel,
}

struct TimedCache<T> {
    refreshed_at: Instant,
    value: Option<T>,
}

static NETWORK_CACHE: OnceLock<Mutex<TimedCache<NetworkSnapshot>>> = OnceLock::new();
static AI_WORK_CACHE: OnceLock<Mutex<TimedCache<AiWorkSnapshot>>> = OnceLock::new();
static STANDBY_CACHE: OnceLock<Mutex<TimedCache<ModernStandbyView>>> = OnceLock::new();
static SAMPLER_STARTED: OnceLock<bool> = OnceLock::new();
static MONITOR_STARTED_AT_MS: OnceLock<i64> = OnceLock::new();
static BACKGROUND_SAMPLER_RUNNING: AtomicBool = AtomicBool::new(false);
static TOKEN_TREND_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static AWAKE_GUARD_ACTIVE: AtomicBool = AtomicBool::new(false);
static AWAKE_GUARD_LAST_UPDATED_MS: AtomicI64 = AtomicI64::new(0);
static AWAKE_GUARD_ERROR: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static SAMPLER_RUNS: AtomicU64 = AtomicU64::new(0);
static LAST_SAMPLER_WALL_US: AtomicU64 = AtomicU64::new(0);
static LAST_SAMPLER_CPU_US: AtomicU64 = AtomicU64::new(0);
static AI_COLLECTIONS: AtomicU64 = AtomicU64::new(0);
static AI_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static LAST_AI_COLLECTION_US: AtomicU64 = AtomicU64::new(0);
static NETWORK_COLLECTIONS: AtomicU64 = AtomicU64::new(0);
static NETWORK_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static LAST_NETWORK_COLLECTION_US: AtomicU64 = AtomicU64::new(0);
static STANDBY_COLLECTIONS: AtomicU64 = AtomicU64::new(0);
static STANDBY_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static LAST_STANDBY_COLLECTION_US: AtomicU64 = AtomicU64::new(0);
static GAP_POWER_EVENT_QUERIES: AtomicU64 = AtomicU64::new(0);
static TOKEN_TREND_WRITES: AtomicU64 = AtomicU64::new(0);
static MANUAL_SPEED_TESTS: AtomicU64 = AtomicU64::new(0);
static MANUAL_DOWNLOAD_BYTES: AtomicU64 = AtomicU64::new(0);
static PROCESS_RESOURCE_BASELINE: OnceLock<Mutex<ProcessResourceBaseline>> = OnceLock::new();
static SYSTEM_CPU_BASELINE: OnceLock<Mutex<SystemCpuBaseline>> = OnceLock::new();

struct ProcessResourceBaseline {
    captured_at: Instant,
    cpu_time_100ns: u64,
    last_cpu_percent: Option<f64>,
    last_interval_secs: Option<f64>,
}

struct SystemCpuBaseline {
    captured_at: Instant,
    idle_time_100ns: u64,
    kernel_time_100ns: u64,
    user_time_100ns: u64,
    last_cpu_percent: Option<f64>,
    last_interval_secs: Option<f64>,
}

struct SamplerLivenessGuard;

impl Drop for SamplerLivenessGuard {
    fn drop(&mut self) {
        if AWAKE_GUARD_ACTIVE.swap(false, Ordering::Relaxed) {
            let _ = set_thread_awake_request(false);
        }
        BACKGROUND_SAMPLER_RUNNING.store(false, Ordering::Relaxed);
    }
}

struct TokenTrendState {
    sampled_at_ms: Option<i64>,
    tokens: HashMap<String, u64>,
    history: VecDeque<TokenTrendPointView>,
    acceptance_events: HashMap<String, AcceptanceEventRecord>,
    history_restored: bool,
    persistence_ok: bool,
    persistence_error: Option<String>,
}

impl Default for TokenTrendState {
    fn default() -> Self {
        Self {
            sampled_at_ms: None,
            tokens: HashMap::new(),
            history: VecDeque::new(),
            acceptance_events: HashMap::new(),
            history_restored: false,
            persistence_ok: false,
            persistence_error: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedTokenTrend {
    schema_version: u32,
    sampled_at_ms: Option<i64>,
    tokens: HashMap<String, u64>,
    history: VecDeque<TokenTrendPointView>,
    #[serde(default)]
    acceptance_events: HashMap<String, AcceptanceEventRecord>,
}

static TOKEN_TREND: OnceLock<Mutex<TokenTrendState>> = OnceLock::new();

/// Start the once-per-minute lightweight sampler. It never calls the network
/// collector and is safe to invoke repeatedly from normal and acceptance mode.
pub fn spawn_sampler() {
    MONITOR_STARTED_AT_MS.get_or_init(unix_time_ms);
    let started = *SAMPLER_STARTED.get_or_init(|| {
        match thread::Builder::new()
            .name("sirin-ai-monitor-sampler".into())
            .spawn(|| {
                let _liveness_guard = SamplerLivenessGuard;
                loop {
                    let wall_started = Instant::now();
                    let cpu_started = current_thread_cpu_time_100ns();
                    let ai_work = cached_ai_work(AI_SAMPLE_MIN_AGE);
                    update_awake_guard(&ai_work);
                    LAST_SAMPLER_WALL_US.store(
                        wall_started.elapsed().as_micros().min(u64::MAX as u128) as u64,
                        Ordering::Relaxed,
                    );
                    if let (Some(before), Some(after)) =
                        (cpu_started, current_thread_cpu_time_100ns())
                    {
                        LAST_SAMPLER_CPU_US
                            .store(after.saturating_sub(before) / 10, Ordering::Relaxed);
                    }
                    SAMPLER_RUNS.fetch_add(1, Ordering::Relaxed);
                    thread::sleep(AI_SAMPLE_INTERVAL);
                }
            }) {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(target: "sirin", "[ai-monitor] sampler start failed: {error}");
                false
            }
        }
    });
    BACKGROUND_SAMPLER_RUNNING.store(started, Ordering::Relaxed);
}

/// Return a read-only monitor snapshot. The expensive network collector and
/// lightweight AI-work collector use independent caches.
pub fn snapshot() -> AiMonitorSnapshot {
    spawn_sampler();
    let started = Instant::now();
    let network = cached_network();
    let ai_work = cached_ai_work(AI_WORK_TTL);
    let codex_health = build_codex_health(&ai_work, &network);
    let power = collect_power(&ai_work);
    update_acceptance_from_power(&power);
    let recovery = collect_recovery(&ai_work, &power);
    let acceptance = collect_monitor_acceptance(&power);
    let overhead = collect_monitor_overhead();
    let alerts = build_local_alerts(&ai_work, &codex_health, &power, &recovery, &overhead);
    AiMonitorSnapshot {
        version: env!("CARGO_PKG_VERSION").to_string(),
        captured_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        collection_ms: started.elapsed().as_millis() as u64,
        cache_ttl_secs: NETWORK_TTL.as_secs(),
        network,
        ai_work,
        codex_health,
        power,
        recovery,
        acceptance,
        overhead,
        alerts,
        safety: SafetyView::default(),
    }
}

fn cached_network() -> NetworkSnapshot {
    let cache = NETWORK_CACHE.get_or_init(|| {
        Mutex::new(TimedCache {
            refreshed_at: Instant::now() - NETWORK_TTL,
            value: None,
        })
    });
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    if guard.refreshed_at.elapsed() < NETWORK_TTL {
        if let Some(value) = &guard.value {
            NETWORK_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return value.clone();
        }
    }
    let started = Instant::now();
    let value = collect_network();
    LAST_NETWORK_COLLECTION_US.store(
        started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    NETWORK_COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    guard.refreshed_at = Instant::now();
    guard.value = Some(value.clone());
    value
}

fn cached_ai_work(max_age: Duration) -> AiWorkSnapshot {
    let cache = AI_WORK_CACHE.get_or_init(|| {
        Mutex::new(TimedCache {
            refreshed_at: Instant::now() - AI_WORK_TTL,
            value: None,
        })
    });
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    if guard.refreshed_at.elapsed() < max_age {
        if let Some(value) = &guard.value {
            AI_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return value.clone();
        }
    }
    let started = Instant::now();
    let value = collect_ai_work();
    LAST_AI_COLLECTION_US.store(
        started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    AI_COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    guard.refreshed_at = Instant::now();
    guard.value = Some(value.clone());
    value
}

fn awake_guard_enabled() -> bool {
    !std::env::var("SIRIN_AI_MONITOR_DISABLE_AWAKE_GUARD")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
}

fn update_awake_guard(ai_work: &AiWorkSnapshot) {
    let chatgpt_running = ai_work
        .processes
        .iter()
        .any(|process| process.app == "ChatGPT" && process.running);
    AWAKE_GUARD_LAST_UPDATED_MS.store(unix_time_ms(), Ordering::Relaxed);

    let desired = awake_guard_enabled() && chatgpt_running;
    let active = AWAKE_GUARD_ACTIVE.load(Ordering::Relaxed);
    if desired == active {
        set_awake_guard_error(None);
        return;
    }
    match set_thread_awake_request(desired) {
        Ok(()) => {
            AWAKE_GUARD_ACTIVE.store(desired, Ordering::Relaxed);
            set_awake_guard_error(None);
        }
        Err(error) => {
            set_awake_guard_error(Some(error.clone()));
            tracing::warn!(target: "sirin", "[ai-monitor] awake guard transition failed: {error}");
        }
    }
}

fn set_awake_guard_error(error: Option<String>) {
    let cell = AWAKE_GUARD_ERROR.get_or_init(|| Mutex::new(None));
    *cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
}

fn awake_guard_error() -> Option<String> {
    AWAKE_GUARD_ERROR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(windows)]
fn set_thread_awake_request(required: bool) -> Result<(), String> {
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    };
    let flags = if required {
        ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    let previous = unsafe { SetThreadExecutionState(flags) };
    if previous.0 == 0 {
        Err(format!(
            "SetThreadExecutionState was rejected: {}",
            std::io::Error::last_os_error()
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_thread_awake_request(_required: bool) -> Result<(), String> {
    Err("Awake guard is currently Windows-only".to_string())
}

fn collect_power(ai_work: &AiWorkSnapshot) -> PowerSnapshot {
    let (session_locked, session_evidence, session_source_error) = collect_session_lock();
    let source_error = awake_guard_error();
    let enabled = awake_guard_enabled();
    let chatgpt_running = ai_work
        .processes
        .iter()
        .any(|process| process.app == "ChatGPT" && process.running);
    let request_active = AWAKE_GUARD_ACTIVE.load(Ordering::Relaxed);
    let last_updated_at_ms = match AWAKE_GUARD_LAST_UPDATED_MS.load(Ordering::Relaxed) {
        0 => None,
        value => Some(value),
    };
    let awake_guard = AwakeGuardView {
        enabled,
        chatgpt_running,
        request_active,
        system_required: request_active,
        display_required: request_active,
        last_updated_at_ms,
        evidence: if source_error.is_none()
            && last_updated_at_ms.is_some()
            && request_active == (enabled && chatgpt_running)
        {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
        source_error,
    };
    let modern_standby = cached_modern_standby();
    let evidence = if session_evidence == EvidenceLevel::Measured
        && awake_guard.evidence == EvidenceLevel::Measured
        && modern_standby.evidence == EvidenceLevel::Measured
    {
        EvidenceLevel::Measured
    } else {
        EvidenceLevel::MissingProof
    };
    PowerSnapshot {
        evidence,
        session_state: match session_locked {
            Some(true) => "LOCKED",
            Some(false) => "UNLOCKED",
            None => "UNKNOWN",
        }
        .to_string(),
        session_locked,
        session_evidence,
        session_source_error,
        awake_guard,
        modern_standby,
    }
}

#[cfg(windows)]
fn collect_session_lock() -> (Option<bool>, EvidenceLevel, Option<String>) {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
        WTS_SESSIONSTATE_UNLOCK,
    };

    let mut buffer = PWSTR::null();
    let mut bytes_returned = 0_u32;
    if let Err(error) = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes_returned,
        )
    } {
        return (
            None,
            EvidenceLevel::MissingProof,
            Some(format!("Query Windows session lock state: {error}")),
        );
    }
    if buffer.is_null() || bytes_returned < std::mem::size_of::<WTSINFOEXW>() as u32 {
        if !buffer.is_null() {
            unsafe { WTSFreeMemory(buffer.0.cast()) };
        }
        return (
            None,
            EvidenceLevel::MissingProof,
            Some("Windows session lock response was incomplete".to_string()),
        );
    }
    let info = unsafe { &*(buffer.0 as *const WTSINFOEXW) };
    let level = info.Level;
    let flags = if level == 1 {
        Some(unsafe { info.Data.WTSInfoExLevel1.SessionFlags })
    } else {
        None
    };
    unsafe { WTSFreeMemory(buffer.0.cast()) };
    match flags.map(|value| value as u32) {
        Some(WTS_SESSIONSTATE_LOCK) => (Some(true), EvidenceLevel::Measured, None),
        Some(WTS_SESSIONSTATE_UNLOCK) => (Some(false), EvidenceLevel::Measured, None),
        Some(value) => (
            None,
            EvidenceLevel::MissingProof,
            Some(format!("Unknown Windows session flag: {value}")),
        ),
        None => (
            None,
            EvidenceLevel::MissingProof,
            Some(format!("Unsupported Windows session info level: {level}")),
        ),
    }
}

#[cfg(not(windows))]
fn collect_session_lock() -> (Option<bool>, EvidenceLevel, Option<String>) {
    (
        None,
        EvidenceLevel::MissingProof,
        Some("Session lock monitoring is currently Windows-only".to_string()),
    )
}

fn cached_modern_standby() -> ModernStandbyView {
    let cache = STANDBY_CACHE.get_or_init(|| {
        Mutex::new(TimedCache {
            refreshed_at: Instant::now() - POWER_TTL,
            value: None,
        })
    });
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    if guard.refreshed_at.elapsed() < POWER_TTL {
        if let Some(value) = &guard.value {
            STANDBY_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return value.clone();
        }
    }
    let started = Instant::now();
    let value = collect_modern_standby();
    LAST_STANDBY_COLLECTION_US.store(
        started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
    STANDBY_COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    guard.refreshed_at = Instant::now();
    guard.value = Some(value.clone());
    value
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PowerEvent {
    id: u32,
    timestamp: String,
    timestamp_ms: i64,
}

#[cfg(windows)]
fn collect_modern_standby() -> ModernStandbyView {
    let query = "*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and (EventID=506 or EventID=507)]]";
    let query_arg = format!("/q:{query}");
    let output = match run_program_utf8(
        "wevtutil.exe",
        &[
            "qe",
            "System",
            &query_arg,
            "/rd:true",
            "/c:16",
            "/f:xml",
            "/uni:false",
        ],
    ) {
        Ok(output) => output,
        Err(error) => {
            return ModernStandbyView {
                evidence: EvidenceLevel::MissingProof,
                source_error: Some(error),
                ..Default::default()
            }
        }
    };
    let events = parse_power_events(&output);
    let entered = events.iter().find(|event| event.id == 506);
    let resumed = events.iter().find(|event| event.id == 507);
    let monitor_started_at_ms = *MONITOR_STARTED_AT_MS.get_or_init(unix_time_ms);
    ModernStandbyView {
        capable: (!events.is_empty()).then_some(true),
        last_entered_at: entered.map(|event| event.timestamp.clone()),
        last_entered_at_ms: entered.map(|event| event.timestamp_ms),
        last_resumed_at: resumed.map(|event| event.timestamp.clone()),
        last_resumed_at_ms: resumed.map(|event| event.timestamp_ms),
        entered_since_monitor_start: entered
            .is_some_and(|event| event.timestamp_ms >= monitor_started_at_ms),
        evidence: EvidenceLevel::Measured,
        source_error: None,
    }
}

#[cfg(not(windows))]
fn collect_modern_standby() -> ModernStandbyView {
    ModernStandbyView {
        evidence: EvidenceLevel::MissingProof,
        source_error: Some("Modern Standby event monitoring is currently Windows-only".to_string()),
        ..Default::default()
    }
}

fn parse_power_events(xml: &str) -> Vec<PowerEvent> {
    static EVENT_RE: OnceLock<regex::Regex> = OnceLock::new();
    static ID_RE: OnceLock<regex::Regex> = OnceLock::new();
    static TIME_RE: OnceLock<regex::Regex> = OnceLock::new();
    let event_re = EVENT_RE
        .get_or_init(|| regex::Regex::new(r"(?s)<Event\b.*?</Event>").expect("power event regex"));
    let id_re = ID_RE.get_or_init(|| {
        regex::Regex::new(r"<EventID>(\d+)</EventID>").expect("power event id regex")
    });
    let time_re = TIME_RE.get_or_init(|| {
        regex::Regex::new(r#"<TimeCreated\s+SystemTime=['"]([^'"]+)['"]"#)
            .expect("power event time regex")
    });

    event_re
        .find_iter(xml)
        .filter_map(|event_match| {
            let event = event_match.as_str();
            let id = id_re
                .captures(event)?
                .get(1)?
                .as_str()
                .parse::<u32>()
                .ok()?;
            if id != 506 && id != 507 {
                return None;
            }
            let raw_time = time_re.captures(event)?.get(1)?.as_str();
            let parsed = chrono::DateTime::parse_from_rfc3339(raw_time).ok()?;
            Some(PowerEvent {
                id,
                timestamp: parsed
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                timestamp_ms: parsed.timestamp_millis(),
            })
        })
        .collect()
}

fn collect_recovery(ai_work: &AiWorkSnapshot, power: &PowerSnapshot) -> RecoverySnapshot {
    let now_ms = unix_time_ms();
    let monitor_started_at_ms = *MONITOR_STARTED_AT_MS.get_or_init(|| now_ms);
    let trend = &ai_work.codex_token_trend;
    let sample_age_secs = trend
        .last_sampled_at_ms
        .map(|sampled_at| now_ms.saturating_sub(sampled_at).max(0) as u64 / 1_000);
    let guard_failed = power.awake_guard.enabled
        && power.awake_guard.chatgpt_running
        && !power.awake_guard.request_active;
    let status = if !BACKGROUND_SAMPLER_RUNNING.load(Ordering::Relaxed)
        || trend.persistence_error.is_some()
    {
        "DEGRADED"
    } else if guard_failed {
        "GUARD_FAILED"
    } else if power.session_locked == Some(true) {
        "LOCKED"
    } else if sample_age_secs.is_some_and(|age| age > AI_SAMPLE_GAP_SECS) {
        "STALE"
    } else if power.modern_standby.entered_since_monitor_start {
        "RECOVERED_AFTER_STANDBY"
    } else if trend.history_restored {
        "RESTORED"
    } else {
        "HEALTHY"
    };
    RecoverySnapshot {
        status: status.to_string(),
        monitor_started_at_ms,
        uptime_secs: now_ms.saturating_sub(monitor_started_at_ms).max(0) as u64 / 1_000,
        last_ai_sample_at_ms: trend.last_sampled_at_ms,
        sample_age_secs,
        token_history_restored: trend.history_restored,
        token_history_persisted: trend.history_persisted,
        evidence: if trend.last_sampled_at_ms.is_some()
            && BACKGROUND_SAMPLER_RUNNING.load(Ordering::Relaxed)
        {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
    }
}

fn update_acceptance_from_power(power: &PowerSnapshot) {
    let Some(cell) = TOKEN_TREND.get() else {
        return;
    };
    let (Some(entered_at_ms), Some(resumed_at_ms)) = (
        power.modern_standby.last_entered_at_ms,
        power.modern_standby.last_resumed_at_ms,
    ) else {
        return;
    };
    if resumed_at_ms < entered_at_ms {
        return;
    }

    let mut state = cell.lock().unwrap_or_else(|error| error.into_inner());
    let correlated_gap = standby_gap_correlated(&state.history, entered_at_ms, resumed_at_ms);
    if correlated_gap && record_acceptance_event(&mut state, "STANDBY_CYCLE", resumed_at_ms) {
        // This writes once per distinct resume event, not on every dashboard
        // refresh. It makes the correlation survive beyond the one-hour chart.
        persist_token_trend_state(&mut state);
    }
}

fn standby_gap_correlated(
    history: &VecDeque<TokenTrendPointView>,
    entered_at_ms: i64,
    resumed_at_ms: i64,
) -> bool {
    if resumed_at_ms < entered_at_ms {
        return false;
    }
    let resume_deadline_ms = resumed_at_ms.saturating_add((AI_SAMPLE_GAP_SECS * 1_000) as i64);
    history.iter().any(|point| {
        point.lifecycle == "GAP"
            && point.sampled_at_ms >= entered_at_ms
            && point.sampled_at_ms <= resume_deadline_ms
    })
}

fn collect_monitor_acceptance(power: &PowerSnapshot) -> MonitorAcceptanceView {
    let Some(cell) = TOKEN_TREND.get() else {
        return MonitorAcceptanceView {
            status: "MISSING_PROOF".to_string(),
            evidence: EvidenceLevel::MissingProof,
            missing_required_modes: vec![
                "IDLE".to_string(),
                "APP_CLOSURE".to_string(),
                "LOCK_CYCLE".to_string(),
                "STANDBY_CYCLE".to_string(),
                "RESTART_RESTORE".to_string(),
            ],
            ..Default::default()
        };
    };
    let state = cell.lock().unwrap_or_else(|error| error.into_inner());
    classify_monitor_acceptance(
        &state.acceptance_events,
        state.persistence_ok,
        state.history_restored,
        power,
    )
}

fn classify_monitor_acceptance(
    events: &HashMap<String, AcceptanceEventRecord>,
    ledger_persisted: bool,
    ledger_restored: bool,
    power: &PowerSnapshot,
) -> MonitorAcceptanceView {
    let idle = simple_acceptance_mode(
        events,
        "TOKEN_IDLE",
        "IDLE",
        true,
        EvidenceLevel::Inferred,
        "已觀察到 AI 程式仍執行但 Token 無增量的區間",
        "尚未觀察到真實閒置區間",
    );

    let apps_closed = events.get("APPS_CLOSED");
    let reopened = latest_acceptance_event(events, &["TOKEN_ACTIVE", "TOKEN_IDLE"]);
    let app_closure = cycle_acceptance_mode(
        "APP_CLOSURE",
        true,
        apps_closed,
        reopened,
        EvidenceLevel::Inferred,
        "已觀察 AI 程式關閉，並在重新開啟後恢復可信取樣",
        "已觀察 AI 程式關閉；尚未觀察重新開啟後的有效取樣",
        "尚未觀察 ChatGPT 與 Codex 同時關閉",
    );

    let source_missing = events.get("SOURCE_MISSING");
    let source_recovered = latest_acceptance_event(
        events,
        &["TOKEN_ACTIVE", "TOKEN_IDLE", "APPS_CLOSED", "SAMPLING_GAP"],
    );
    let token_source = cycle_acceptance_mode(
        "TOKEN_SOURCE_RECOVERY",
        false,
        source_missing,
        source_recovered,
        EvidenceLevel::Inferred,
        "Token 來源曾不可讀，恢復後已重新建立可信基線",
        "Token 來源目前有缺口；尚未觀察恢復",
        "尚未發生 Token 來源不可讀事件",
    );

    let locked = events.get("WINDOWS_LOCKED");
    let unlocked = events.get("WINDOWS_UNLOCKED");
    let lock_cycle = cycle_acceptance_mode(
        "LOCK_CYCLE",
        true,
        locked,
        unlocked,
        EvidenceLevel::Measured,
        "已由 Win32 直接觀察鎖定與其後解鎖",
        "已觀察鎖定；尚未觀察其後解鎖",
        "尚未觀察真實 Windows 鎖定",
    );

    let standby_cycle = if let Some(record) = events.get("STANDBY_CYCLE") {
        AcceptanceModeView {
            mode: "STANDBY_CYCLE".to_string(),
            required: true,
            status: "PASS".to_string(),
            last_observed_at_ms: Some(record.last_observed_at_ms),
            observations: record.observations,
            evidence: EvidenceLevel::Measured,
            detail: "Kernel-Power 待機／恢復事件已與 Token 取樣 GAP 對齊".to_string(),
        }
    } else if let (Some(entered), Some(resumed)) = (
        power.modern_standby.last_entered_at_ms,
        power.modern_standby.last_resumed_at_ms,
    ) {
        AcceptanceModeView {
            mode: "STANDBY_CYCLE".to_string(),
            required: true,
            status: "PARTIAL".to_string(),
            last_observed_at_ms: Some(resumed.max(entered)),
            observations: 1,
            evidence: EvidenceLevel::MissingProof,
            detail: "已有歷史待機事件，但尚未與目前持久化 Token GAP 對齊".to_string(),
        }
    } else {
        AcceptanceModeView {
            mode: "STANDBY_CYCLE".to_string(),
            required: true,
            status: "MISSING_PROOF".to_string(),
            evidence: EvidenceLevel::MissingProof,
            detail: "尚未觀察可對齊的真實 Modern Standby／恢復週期".to_string(),
            ..Default::default()
        }
    };

    let restart_restore = simple_acceptance_mode(
        events,
        "HISTORY_RESTORED",
        "RESTART_RESTORE",
        true,
        EvidenceLevel::Measured,
        "Sirin 重啟後已從本機趨勢檔恢復歷史",
        "尚未證明 Sirin 重啟後可恢復 Token 歷史",
    );

    let modes = vec![
        idle,
        app_closure,
        lock_cycle,
        standby_cycle,
        restart_restore,
        token_source,
    ];
    let missing_required_modes = modes
        .iter()
        .filter(|mode| mode.required && mode.status != "PASS")
        .map(|mode| mode.mode.clone())
        .collect::<Vec<_>>();
    MonitorAcceptanceView {
        status: if missing_required_modes.is_empty() {
            "PASS"
        } else {
            "PARTIAL"
        }
        .to_string(),
        evidence: if missing_required_modes.is_empty() {
            EvidenceLevel::Inferred
        } else {
            EvidenceLevel::MissingProof
        },
        modes,
        missing_required_modes,
        ledger_persisted,
        ledger_restored,
    }
}

fn simple_acceptance_mode(
    events: &HashMap<String, AcceptanceEventRecord>,
    event_kind: &str,
    mode: &str,
    required: bool,
    pass_evidence: EvidenceLevel,
    pass_detail: &str,
    missing_detail: &str,
) -> AcceptanceModeView {
    match events.get(event_kind) {
        Some(record) => AcceptanceModeView {
            mode: mode.to_string(),
            required,
            status: "PASS".to_string(),
            last_observed_at_ms: Some(record.last_observed_at_ms),
            observations: record.observations,
            evidence: pass_evidence,
            detail: pass_detail.to_string(),
        },
        None => AcceptanceModeView {
            mode: mode.to_string(),
            required,
            status: "MISSING_PROOF".to_string(),
            evidence: EvidenceLevel::MissingProof,
            detail: missing_detail.to_string(),
            ..Default::default()
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn cycle_acceptance_mode(
    mode: &str,
    required: bool,
    start: Option<&AcceptanceEventRecord>,
    recovered: Option<&AcceptanceEventRecord>,
    pass_evidence: EvidenceLevel,
    pass_detail: &str,
    partial_detail: &str,
    missing_detail: &str,
) -> AcceptanceModeView {
    match start {
        Some(start)
            if recovered
                .is_some_and(|record| record.last_observed_at_ms > start.last_observed_at_ms) =>
        {
            let recovered = recovered.expect("recovery record checked");
            AcceptanceModeView {
                mode: mode.to_string(),
                required,
                status: "PASS".to_string(),
                last_observed_at_ms: Some(recovered.last_observed_at_ms),
                observations: start.observations,
                evidence: pass_evidence,
                detail: pass_detail.to_string(),
            }
        }
        Some(start) => AcceptanceModeView {
            mode: mode.to_string(),
            required,
            status: "PARTIAL".to_string(),
            last_observed_at_ms: Some(start.last_observed_at_ms),
            observations: start.observations,
            evidence: EvidenceLevel::MissingProof,
            detail: partial_detail.to_string(),
        },
        None => AcceptanceModeView {
            mode: mode.to_string(),
            required,
            status: "MISSING_PROOF".to_string(),
            evidence: EvidenceLevel::MissingProof,
            detail: missing_detail.to_string(),
            ..Default::default()
        },
    }
}

fn latest_acceptance_event<'a>(
    events: &'a HashMap<String, AcceptanceEventRecord>,
    kinds: &[&str],
) -> Option<&'a AcceptanceEventRecord> {
    kinds
        .iter()
        .filter_map(|kind| events.get(*kind))
        .max_by_key(|record| record.last_observed_at_ms)
}

fn collect_monitor_overhead() -> MonitorOverheadSnapshot {
    let process = collect_sirin_process_resources();
    let trend_file_bytes = std::fs::metadata(token_trend_path())
        .ok()
        .map(|metadata| metadata.len());
    MonitorOverheadSnapshot {
        evidence: if SAMPLER_RUNS.load(Ordering::Relaxed) > 0
            && process.memory_evidence == EvidenceLevel::Measured
        {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
        scope: "AI_MONITOR_COUNTERS_AND_ENTIRE_SIRIN_PROCESS_RESOURCES".to_string(),
        sampler_interval_secs: AI_SAMPLE_INTERVAL.as_secs(),
        network_cache_ttl_secs: NETWORK_TTL.as_secs(),
        power_cache_ttl_secs: POWER_TTL.as_secs(),
        ai_cache_ttl_secs: AI_WORK_TTL.as_secs(),
        sampler_runs_total: SAMPLER_RUNS.load(Ordering::Relaxed),
        last_sampler_wall_ms: micros_to_millis(LAST_SAMPLER_WALL_US.load(Ordering::Relaxed)),
        last_sampler_cpu_ms: atomic_optional_micros(&LAST_SAMPLER_CPU_US),
        ai_collections_total: AI_COLLECTIONS.load(Ordering::Relaxed),
        ai_cache_hits_total: AI_CACHE_HITS.load(Ordering::Relaxed),
        last_ai_collection_ms: micros_to_millis(LAST_AI_COLLECTION_US.load(Ordering::Relaxed)),
        network_collections_total: NETWORK_COLLECTIONS.load(Ordering::Relaxed),
        network_cache_hits_total: NETWORK_CACHE_HITS.load(Ordering::Relaxed),
        last_network_collection_ms: micros_to_millis(
            LAST_NETWORK_COLLECTION_US.load(Ordering::Relaxed),
        ),
        standby_collections_total: STANDBY_COLLECTIONS.load(Ordering::Relaxed),
        standby_cache_hits_total: STANDBY_CACHE_HITS.load(Ordering::Relaxed),
        last_standby_collection_ms: micros_to_millis(
            LAST_STANDBY_COLLECTION_US.load(Ordering::Relaxed),
        ),
        gap_power_event_queries_total: GAP_POWER_EVENT_QUERIES.load(Ordering::Relaxed),
        trend_writes_total: TOKEN_TREND_WRITES.load(Ordering::Relaxed),
        trend_file_bytes,
        manual_speed_tests_total: MANUAL_SPEED_TESTS.load(Ordering::Relaxed),
        manual_download_bytes_total: MANUAL_DOWNLOAD_BYTES.load(Ordering::Relaxed),
        background_network_probes: false,
        background_download_tests: false,
        process,
    }
}

fn micros_to_millis(value: u64) -> f64 {
    value as f64 / 1_000.0
}

fn atomic_optional_micros(value: &AtomicU64) -> Option<f64> {
    match value.load(Ordering::Relaxed) {
        0 => None,
        value => Some(micros_to_millis(value)),
    }
}

#[cfg(windows)]
fn collect_sirin_process_resources() -> SirinProcessResourceView {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let process = unsafe { GetCurrentProcess() };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let cpu_time_100ns =
        match unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) }
        {
            Ok(()) => Some(filetime_ticks(kernel).saturating_add(filetime_ticks(user))),
            Err(_) => None,
        };

    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    let memory_ok = unsafe {
        K32GetProcessMemoryInfo(
            process,
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
        .as_bool()
    };

    let (cpu_percent_recent, cpu_interval_secs, cpu_evidence) =
        if let Some(cpu_time_100ns) = cpu_time_100ns {
            let now = Instant::now();
            let cell = PROCESS_RESOURCE_BASELINE.get_or_init(|| {
                Mutex::new(ProcessResourceBaseline {
                    captured_at: now,
                    cpu_time_100ns,
                    last_cpu_percent: None,
                    last_interval_secs: None,
                })
            });
            let mut baseline = cell.lock().unwrap_or_else(|error| error.into_inner());
            let elapsed = now.saturating_duration_since(baseline.captured_at);
            if elapsed >= Duration::from_secs(5) {
                let cpu_seconds =
                    cpu_time_100ns.saturating_sub(baseline.cpu_time_100ns) as f64 / 10_000_000.0;
                let processors = std::thread::available_parallelism()
                    .map(|count| count.get())
                    .unwrap_or(1) as f64;
                baseline.last_cpu_percent =
                    Some((cpu_seconds / elapsed.as_secs_f64() / processors * 100.0).max(0.0));
                baseline.last_interval_secs = Some(elapsed.as_secs_f64());
                baseline.captured_at = now;
                baseline.cpu_time_100ns = cpu_time_100ns;
            }
            (
                baseline.last_cpu_percent,
                baseline.last_interval_secs,
                if baseline.last_cpu_percent.is_some() {
                    EvidenceLevel::Measured
                } else {
                    EvidenceLevel::MissingProof
                },
            )
        } else {
            (None, None, EvidenceLevel::MissingProof)
        };

    SirinProcessResourceView {
        scope: "ENTIRE_SIRIN_PROCESS_NOT_MONITOR_ONLY".to_string(),
        cpu_percent_recent,
        cpu_interval_secs,
        cpu_evidence,
        working_set_mb: memory_ok.then_some(counters.WorkingSetSize as f64 / 1_048_576.0),
        private_mb: memory_ok.then_some(counters.PrivateUsage as f64 / 1_048_576.0),
        memory_evidence: if memory_ok {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
        source_error: (!memory_ok).then(|| "Read Sirin process memory counters failed".to_string()),
    }
}

#[cfg(not(windows))]
fn collect_sirin_process_resources() -> SirinProcessResourceView {
    SirinProcessResourceView {
        scope: "ENTIRE_SIRIN_PROCESS_NOT_MONITOR_ONLY".to_string(),
        source_error: Some("Sirin process resource monitoring is currently Windows-only".into()),
        ..Default::default()
    }
}

#[cfg(windows)]
fn current_thread_cpu_time_100ns() -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    Some(filetime_ticks(kernel).saturating_add(filetime_ticks(user)))
}

#[cfg(not(windows))]
fn current_thread_cpu_time_100ns() -> Option<u64> {
    None
}

#[cfg(windows)]
fn filetime_ticks(value: windows::Win32::Foundation::FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

#[cfg(windows)]
fn collect_system_resources(processes: &[ProcessGroupView]) -> SystemResourceView {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::Win32::System::SystemInformation::{
        GetWindowsDirectoryW, GlobalMemoryStatusEx, MEMORYSTATUSEX,
    };
    use windows::Win32::System::Threading::GetSystemTimes;

    let mut errors = Vec::new();
    let now = Instant::now();
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let (cpu_percent_recent, cpu_interval_secs, cpu_evidence) = match unsafe {
        GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user))
    } {
        Ok(()) => {
            let idle_time_100ns = filetime_ticks(idle);
            let kernel_time_100ns = filetime_ticks(kernel);
            let user_time_100ns = filetime_ticks(user);
            let cell = SYSTEM_CPU_BASELINE.get_or_init(|| {
                Mutex::new(SystemCpuBaseline {
                    captured_at: now,
                    idle_time_100ns,
                    kernel_time_100ns,
                    user_time_100ns,
                    last_cpu_percent: None,
                    last_interval_secs: None,
                })
            });
            let mut baseline = cell.lock().unwrap_or_else(|error| error.into_inner());
            let elapsed = now.saturating_duration_since(baseline.captured_at);
            if elapsed >= Duration::from_secs(30) {
                let idle_delta = idle_time_100ns.saturating_sub(baseline.idle_time_100ns);
                let kernel_delta = kernel_time_100ns.saturating_sub(baseline.kernel_time_100ns);
                let user_delta = user_time_100ns.saturating_sub(baseline.user_time_100ns);
                let total_delta = kernel_delta.saturating_add(user_delta);
                if total_delta > 0 {
                    let busy_delta = total_delta.saturating_sub(idle_delta);
                    baseline.last_cpu_percent =
                        Some((busy_delta as f64 / total_delta as f64 * 100.0).clamp(0.0, 100.0));
                    baseline.last_interval_secs = Some(elapsed.as_secs_f64());
                }
                baseline.captured_at = now;
                baseline.idle_time_100ns = idle_time_100ns;
                baseline.kernel_time_100ns = kernel_time_100ns;
                baseline.user_time_100ns = user_time_100ns;
            }
            (
                baseline.last_cpu_percent,
                baseline.last_interval_secs,
                if baseline.last_cpu_percent.is_some() {
                    EvidenceLevel::Measured
                } else {
                    EvidenceLevel::MissingProof
                },
            )
        }
        Err(error) => {
            errors.push(format!("Read Windows CPU counters: {error}"));
            (None, None, EvidenceLevel::MissingProof)
        }
    };

    let mut memory = MEMORYSTATUSEX::default();
    memory.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let memory_ok = match unsafe { GlobalMemoryStatusEx(&mut memory) } {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("Read Windows memory counters: {error}"));
            false
        }
    };
    let physical_memory_total_gb =
        memory_ok.then_some(memory.ullTotalPhys as f64 / 1_073_741_824.0);
    let physical_memory_available_gb =
        memory_ok.then_some(memory.ullAvailPhys as f64 / 1_073_741_824.0);
    let physical_memory_available_percent = (memory_ok && memory.ullTotalPhys > 0)
        .then_some(memory.ullAvailPhys as f64 / memory.ullTotalPhys as f64 * 100.0);
    let committed_percent = (memory_ok && memory.ullTotalPageFile > 0).then_some(
        memory
            .ullTotalPageFile
            .saturating_sub(memory.ullAvailPageFile) as f64
            / memory.ullTotalPageFile as f64
            * 100.0,
    );

    let mut windows_directory = [0_u16; 260];
    let windows_directory_len = unsafe { GetWindowsDirectoryW(Some(&mut windows_directory)) };
    let windows_directory = String::from_utf16_lossy(
        &windows_directory[..(windows_directory_len as usize).min(windows_directory.len())],
    );
    let system_drive = windows_directory
        .get(..2)
        .filter(|drive| drive.ends_with(':'))
        .unwrap_or("C:")
        .to_string();
    let drive_root = HSTRING::from(format!("{}\\", system_drive.trim_end_matches('\\')));
    let mut available_to_caller = 0_u64;
    let mut total_bytes = 0_u64;
    let mut total_free_bytes = 0_u64;
    let disk_ok = match unsafe {
        GetDiskFreeSpaceExW(
            &drive_root,
            Some(&mut available_to_caller),
            Some(&mut total_bytes),
            Some(&mut total_free_bytes),
        )
    } {
        Ok(()) => true,
        Err(error) => {
            errors.push(format!("Read system drive capacity: {error}"));
            false
        }
    };
    let system_drive_free_gb = disk_ok.then_some(total_free_bytes as f64 / 1_073_741_824.0);
    let system_drive_free_percent = (disk_ok && total_bytes > 0)
        .then_some(total_free_bytes as f64 / total_bytes as f64 * 100.0);
    let ai_working_set_mb = processes
        .iter()
        .filter(|process| process.app == "ChatGPT" || process.app == "Codex")
        .map(|process| process.working_set_mb)
        .sum();

    let mut view = SystemResourceView {
        evidence: if memory_ok && disk_ok {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
        cpu_percent_recent,
        cpu_interval_secs,
        cpu_evidence,
        physical_memory_total_gb,
        physical_memory_available_gb,
        physical_memory_available_percent,
        committed_percent,
        memory_evidence: if memory_ok {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
        system_drive: disk_ok.then_some(system_drive),
        system_drive_free_gb,
        system_drive_free_percent,
        disk_evidence: if disk_ok {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
        ai_working_set_mb,
        pressure: String::new(),
        pressure_reasons: Vec::new(),
        source_error: (!errors.is_empty()).then(|| errors.join("; ")),
    };
    let (pressure, reasons) = classify_system_pressure(&view);
    view.pressure = pressure;
    view.pressure_reasons = reasons;
    view
}

#[cfg(not(windows))]
fn collect_system_resources(processes: &[ProcessGroupView]) -> SystemResourceView {
    SystemResourceView {
        ai_working_set_mb: processes
            .iter()
            .filter(|process| process.app == "ChatGPT" || process.app == "Codex")
            .map(|process| process.working_set_mb)
            .sum(),
        pressure: "MISSING_PROOF".to_string(),
        source_error: Some("System resource monitoring is currently Windows-only".to_string()),
        ..Default::default()
    }
}

fn classify_system_pressure(resources: &SystemResourceView) -> (String, Vec<String>) {
    let mut critical = Vec::new();
    let mut warning = Vec::new();
    if resources
        .cpu_percent_recent
        .is_some_and(|percent| percent >= 98.0)
    {
        critical.push("CPU 最近區間達 98% 以上".to_string());
    } else if resources
        .cpu_percent_recent
        .is_some_and(|percent| percent >= 90.0)
    {
        warning.push("CPU 最近區間達 90% 以上".to_string());
    }
    if resources
        .physical_memory_available_percent
        .is_some_and(|percent| percent < 8.0)
        || resources
            .committed_percent
            .is_some_and(|percent| percent >= 95.0)
    {
        critical.push("可用記憶體低於 8% 或已認可記憶體達 95%".to_string());
    } else if resources
        .physical_memory_available_percent
        .is_some_and(|percent| percent < 15.0)
        || resources
            .committed_percent
            .is_some_and(|percent| percent >= 90.0)
    {
        warning.push("可用記憶體低於 15% 或已認可記憶體達 90%".to_string());
    }
    if resources.system_drive_free_gb.is_some_and(|gb| gb < 5.0)
        || resources
            .system_drive_free_percent
            .is_some_and(|percent| percent < 3.0)
    {
        critical.push("系統碟可用空間低於 5 GB 或 3%".to_string());
    } else if resources.system_drive_free_gb.is_some_and(|gb| gb < 15.0)
        || resources
            .system_drive_free_percent
            .is_some_and(|percent| percent < 8.0)
    {
        warning.push("系統碟可用空間低於 15 GB 或 8%".to_string());
    }
    if !critical.is_empty() {
        critical.extend(warning);
        ("CRITICAL".to_string(), critical)
    } else if !warning.is_empty() {
        ("WARNING".to_string(), warning)
    } else if resources.memory_evidence == EvidenceLevel::Measured
        && resources.disk_evidence == EvidenceLevel::Measured
    {
        ("HEALTHY".to_string(), Vec::new())
    } else {
        ("MISSING_PROOF".to_string(), Vec::new())
    }
}

fn build_codex_health(ai_work: &AiWorkSnapshot, network: &NetworkSnapshot) -> CodexHealthView {
    let resources = &ai_work.system_resources;
    let codex_running = process_group_running(&ai_work.processes, "Codex");
    let active_task_count = ai_work
        .codex_tasks
        .iter()
        .filter(|task| task.activity == "ACTIVE_RECENTLY")
        .count() as u32;
    let progress_age_secs = ai_work
        .codex_tasks
        .first()
        .map(|task| unix_time_ms().saturating_sub(task.updated_at_ms).max(0) as u64 / 1_000);
    let (network_status, network_summary) = classify_network_health(network);
    let resource_summary = format_system_resource_summary(resources);

    let (status, severity, label, detail, evidence, progress_status) =
        if ai_work.codex_source_error.is_some() || ai_work.process_source_error.is_some() {
            (
                "MISSING_PROOF",
                "UNKNOWN",
                "Codex 健康狀態缺證據",
                "本機程序或 Token 資料來源不可讀；不推定為正常或故障".to_string(),
                EvidenceLevel::MissingProof,
                "MISSING_PROOF",
            )
        } else if resources.pressure == "CRITICAL" {
            (
                "LOCAL_RESOURCE_PRESSURE",
                "CRITICAL",
                "本機資源受限",
                resources.pressure_reasons.join("；"),
                EvidenceLevel::Measured,
                "RESOURCE_LIMITED",
            )
        } else if resources.pressure == "WARNING" {
            (
                "LOCAL_RESOURCE_PRESSURE",
                "WARNING",
                "本機資源偏高",
                resources.pressure_reasons.join("；"),
                EvidenceLevel::Measured,
                "RESOURCE_LIMITED",
            )
        } else if !codex_running {
            (
                "CODEX_CLOSED",
                "NEUTRAL",
                "Codex 未執行",
                "沒有偵測到 Codex 程序；這不是卡住證據".to_string(),
                EvidenceLevel::Measured,
                "IDLE",
            )
        } else if ai_work.codex_token_trend.delta_tokens > 0 && active_task_count > 0 {
            (
                "HEALTHY_ACTIVITY",
                "OK",
                "本機健康 · 有進度",
                format!(
                    "{} 項近期活動；最近取樣增加 {} tokens",
                    active_task_count, ai_work.codex_token_trend.delta_tokens
                ),
                EvidenceLevel::Inferred,
                "PROGRESS_OBSERVED",
            )
        } else if active_task_count > 0 {
            (
                "RECENT_ACTIVITY",
                "OK",
                "本機健康 · 最近有活動",
                "工作時間戳仍在更新；本次 Token 取樣沒有增量".to_string(),
                EvidenceLevel::Inferred,
                "RECENT_UPDATE",
            )
        } else {
            (
                "WAITING_OR_IDLE",
                "UNKNOWN",
                "等待或暫停",
                "沒有新進度證據；無法區分正常等待、使用者決定或停滯".to_string(),
                EvidenceLevel::MissingProof,
                "SUSPECTED_STALL_OR_WAITING",
            )
        };

    CodexHealthView {
        status: status.to_string(),
        severity: severity.to_string(),
        label: label.to_string(),
        detail,
        evidence,
        progress_status: progress_status.to_string(),
        progress_age_secs,
        active_task_count,
        token_delta: ai_work.codex_token_trend.delta_tokens,
        local_resource_status: resources.pressure.clone(),
        resource_summary,
        network_status,
        network_summary,
        remote_limit_status: "MISSING_PROOF".to_string(),
        remote_limit_evidence: EvidenceLevel::MissingProof,
    }
}

fn classify_network_health(network: &NetworkSnapshot) -> (String, String) {
    let ipv4 = network
        .default_routes
        .iter()
        .find(|route| route.selected && route.family == "IPv4");
    let Some(ipv4) = ipv4 else {
        return (
            "MISSING_PROOF".to_string(),
            "IPv4 預設路由缺證據".to_string(),
        );
    };
    let latency = ipv4
        .latency_ms
        .map(|ms| format!("{} ms", ms))
        .unwrap_or_else(|| "無回覆".to_string());
    let summary = if network.split_default_route {
        format!(
            "IPv4 {} · {}；IPv6 分流至其他介面",
            ipv4.interface_alias, latency
        )
    } else {
        format!("IPv4 {} · {}", ipv4.interface_alias, latency)
    };
    if ipv4.latency_status == "GOOD" {
        (
            if network.split_default_route {
                "SPLIT_ROUTE_WARNING"
            } else {
                "GOOD"
            }
            .to_string(),
            summary,
        )
    } else {
        ("WARNING".to_string(), summary)
    }
}

fn format_system_resource_summary(resources: &SystemResourceView) -> String {
    let cpu = resources
        .cpu_percent_recent
        .map(|value| format!("CPU {:.0}%", value))
        .unwrap_or_else(|| "CPU 建立基線中".to_string());
    let memory = resources
        .physical_memory_available_percent
        .map(|value| format!("RAM 可用 {:.0}%", value))
        .unwrap_or_else(|| "RAM 缺證據".to_string());
    let drive = match (&resources.system_drive, resources.system_drive_free_gb) {
        (Some(drive), Some(gb)) => format!("{} 可用 {:.0} GB", drive, gb),
        _ => "系統碟缺證據".to_string(),
    };
    format!(
        "{} · {} · {} · AI 程序 {:.0} MB",
        cpu, memory, drive, resources.ai_working_set_mb
    )
}

fn build_local_alerts(
    ai_work: &AiWorkSnapshot,
    codex_health: &CodexHealthView,
    power: &PowerSnapshot,
    recovery: &RecoverySnapshot,
    overhead: &MonitorOverheadSnapshot,
) -> Vec<LocalAlertView> {
    let mut alerts = Vec::new();
    if codex_health.local_resource_status == "CRITICAL"
        || codex_health.local_resource_status == "WARNING"
    {
        alerts.push(LocalAlertView {
            code: "CODEX_LOCAL_RESOURCE_PRESSURE".to_string(),
            severity: if codex_health.local_resource_status == "CRITICAL" {
                "CRITICAL"
            } else {
                "WARNING"
            }
            .to_string(),
            message: codex_health.label.clone(),
            action: "先確認 CPU、記憶體與系統碟壓力；不自動終止程序或清理檔案".to_string(),
            evidence: EvidenceLevel::Measured,
        });
    }
    if power.session_locked == Some(true) {
        alerts.push(LocalAlertView {
            code: "WINDOWS_SESSION_LOCKED".to_string(),
            severity: "WARNING".to_string(),
            message: "Windows 已鎖定；需要桌面畫面的 AI 操作會暫停".to_string(),
            action: "由使用者解鎖 Windows；不要嘗試繞過登入".to_string(),
            evidence: EvidenceLevel::Measured,
        });
    }
    if power.awake_guard.enabled
        && power.awake_guard.chatgpt_running
        && !power.awake_guard.request_active
    {
        alerts.push(LocalAlertView {
            code: "AWAKE_GUARD_FAILED".to_string(),
            severity: "CRITICAL".to_string(),
            message: "ChatGPT 正在執行，但 Sirin 沒有取得待機防護".to_string(),
            action: "檢查 Sirin 日誌；必要時暫時恢復舊 PowerShell 守護".to_string(),
            evidence: power.awake_guard.evidence,
        });
    }
    if let Some(error) = &ai_work.codex_token_trend.persistence_error {
        alerts.push(LocalAlertView {
            code: "TOKEN_HISTORY_SAVE_FAILED".to_string(),
            severity: "WARNING".to_string(),
            message: "Token 趨勢無法保存，重啟後可能中斷".to_string(),
            action: format!("檢查 Sirin tracking 目錄：{error}"),
            evidence: EvidenceLevel::Measured,
        });
    }
    if let Some(error) = &ai_work.codex_source_error {
        alerts.push(LocalAlertView {
            code: "TOKEN_SOURCE_MISSING".to_string(),
            severity: "WARNING".to_string(),
            message: "Codex Token 資料來源目前不可讀".to_string(),
            action: format!("保留上一個有效基線並停止推算速率：{error}"),
            evidence: EvidenceLevel::MissingProof,
        });
    }
    if recovery
        .sample_age_secs
        .is_some_and(|age| age > AI_SAMPLE_GAP_SECS)
    {
        alerts.push(LocalAlertView {
            code: "AI_SAMPLE_STALE".to_string(),
            severity: "WARNING".to_string(),
            message: "AI 工作取樣已超過 90 秒沒有更新".to_string(),
            action: "確認 Sirin 背景取樣仍在執行，並檢查是否剛從待機恢復".to_string(),
            evidence: recovery.evidence,
        });
    }
    let now_ms = unix_time_ms();
    if power.modern_standby.entered_since_monitor_start
        && power
            .modern_standby
            .last_entered_at_ms
            .is_some_and(|entered| now_ms.saturating_sub(entered) <= 10 * 60 * 1_000)
    {
        alerts.push(LocalAlertView {
            code: "RECENT_MODERN_STANDBY".to_string(),
            severity: "INFO".to_string(),
            message: "最近 10 分鐘內曾進入 Modern Standby".to_string(),
            action: "確認 Token 圖上的中斷區段與桌面操作恢復狀態".to_string(),
            evidence: power.modern_standby.evidence,
        });
    }
    if overhead.last_sampler_wall_ms > 5_000.0
        || overhead
            .last_sampler_cpu_ms
            .is_some_and(|value| value > 500.0)
    {
        alerts.push(LocalAlertView {
            code: "MONITOR_SAMPLER_SLOW".to_string(),
            severity: "WARNING".to_string(),
            message: "AI 背景取樣超出低負擔預算".to_string(),
            action: "檢查 Codex 狀態資料庫與程序查詢是否變慢".to_string(),
            evidence: overhead.evidence,
        });
    }
    if overhead.last_network_collection_ms > 5_000.0 {
        alerts.push(LocalAlertView {
            code: "MONITOR_NETWORK_COLLECTION_SLOW".to_string(),
            severity: "WARNING".to_string(),
            message: "本機網路證據收集超過 5 秒".to_string(),
            action: "檢查 netsh 或 ICMP 是否被系統網路堆疊延遲".to_string(),
            evidence: EvidenceLevel::Measured,
        });
    }
    if overhead
        .trend_file_bytes
        .is_some_and(|bytes| bytes > 1_048_576)
    {
        alerts.push(LocalAlertView {
            code: "TOKEN_TREND_FILE_TOO_LARGE".to_string(),
            severity: "WARNING".to_string(),
            message: "Token 趨勢檔超出 1 MB 安全界線".to_string(),
            action: "檢查一小時／60 點的保留上限是否失效；不要自動刪除檔案".to_string(),
            evidence: EvidenceLevel::Measured,
        });
    }
    alerts
}

/// Explicit, user-triggered 5 MB download measurement. This is never called
/// by the snapshot path and therefore never runs in the background.
pub async fn manual_speed_test() -> Result<SpeedTestView, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("Sirin-AI-Work-Monitor/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("speed test client: {e}"))?;
    let started = Instant::now();
    let response = client
        .get(SPEED_TEST_URL)
        .send()
        .await
        .map_err(|e| format!("speed test request: {e}"))?
        .error_for_status()
        .map_err(|e| format!("speed test response: {e}"))?;
    let mut stream = response.bytes_stream();
    let mut bytes_downloaded = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("speed test stream: {e}"))?;
        bytes_downloaded = bytes_downloaded.saturating_add(chunk.len() as u64);
        if bytes_downloaded >= SPEED_TEST_BYTES {
            break;
        }
    }
    if bytes_downloaded == 0 {
        return Err("speed test returned no bytes".to_string());
    }
    let elapsed = started.elapsed();
    let elapsed_secs = elapsed.as_secs_f64().max(0.001);
    MANUAL_SPEED_TESTS.fetch_add(1, Ordering::Relaxed);
    MANUAL_DOWNLOAD_BYTES.fetch_add(bytes_downloaded, Ordering::Relaxed);
    Ok(SpeedTestView {
        captured_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        endpoint: SPEED_TEST_URL.to_string(),
        bytes_downloaded,
        elapsed_ms: elapsed.as_millis() as u64,
        megabits_per_second: bytes_downloaded as f64 * 8.0 / elapsed_secs / 1_000_000.0,
        manual_only: true,
        evidence: EvidenceLevel::Measured,
    })
}

fn collect_ai_work() -> AiWorkSnapshot {
    let (processes, process_error) = collect_processes();
    let system_resources = collect_system_resources(&processes);
    let (tasks, codex_error) = collect_codex_tasks();
    let recent_total = tasks.iter().map(|task| task.tokens_used).sum();
    let chatgpt_running = process_group_running(&processes, "ChatGPT");
    let codex_running = process_group_running(&processes, "Codex");
    // WTSQuerySessionInformationW is an in-process, local Win32 read. Recording
    // it with the existing minute sampler preserves a real lock/unlock cycle
    // even when neither the Web UI nor the Widgets board is open.
    let (session_locked, _, _) = collect_session_lock();
    let trend = update_token_trend(
        &tasks,
        codex_error.is_none(),
        chatgpt_running,
        codex_running,
        session_locked,
    );
    if trend.lifecycle == "GAP" {
        // A gap is rare and is the only background condition that justifies a
        // local event-log query. Correlating immediately prevents standby proof
        // from disappearing if the UI remains closed for more than an hour.
        GAP_POWER_EVENT_QUERIES.fetch_add(1, Ordering::Relaxed);
        let modern_standby = cached_modern_standby();
        update_acceptance_from_power(&PowerSnapshot {
            modern_standby,
            ..Default::default()
        });
    }
    let sirin = crate::multi_agent::usage::snapshot(300);

    AiWorkSnapshot {
        evidence: if process_error.is_none() && codex_error.is_none() {
            EvidenceLevel::Measured
        } else {
            EvidenceLevel::MissingProof
        },
        processes,
        codex_tasks: tasks,
        codex_recent_total_tokens: recent_total,
        codex_token_trend: trend,
        codex_source: "%USERPROFILE%\\.codex\\state_5.sqlite / threads.tokens_used (read-only internal counter; not billing)".to_string(),
        codex_source_error: codex_error,
        process_source_error: process_error,
        system_resources,
        sirin_tokens: SirinTokenView {
            window_secs: sirin.window_secs,
            api_calls: sirin.api_calls,
            total_tokens: sirin.total_tokens,
            tokens_per_min: sirin.tokens_per_min,
            cache_hit_pct: sirin.cache_hit_pct,
            estimated_cost_per_hour_usd: sirin.cost_per_hour,
            evidence: if sirin.api_calls > 0 {
                EvidenceLevel::Measured
            } else {
                // The existing collector returns zero both when usage is truly
                // zero and when no worker session logs can be located.
                EvidenceLevel::MissingProof
            },
        },
    }
}

fn process_group_running(processes: &[ProcessGroupView], app: &str) -> bool {
    processes
        .iter()
        .any(|process| process.app == app && process.running)
}

fn collect_codex_tasks() -> (Vec<CodexTaskView>, Option<String>) {
    let Some(home) = local_user_home_dir() else {
        return (
            Vec::new(),
            Some("Cannot resolve the local user home directory".into()),
        );
    };
    let db = home.join(".codex").join("state_5.sqlite");
    if !db.is_file() {
        return (
            Vec::new(),
            Some(format!("Codex state database not found: {}", db.display())),
        );
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match Connection::open_with_flags(&db, flags) {
        Ok(conn) => conn,
        Err(e) => return (Vec::new(), Some(format!("Open Codex state database: {e}"))),
    };
    let _ = conn.busy_timeout(Duration::from_millis(300));
    let mut stmt = match conn.prepare(
        "SELECT id, tokens_used, updated_at_ms \
         FROM threads WHERE archived = 0 \
         ORDER BY updated_at_ms DESC LIMIT ?1",
    ) {
        Ok(stmt) => stmt,
        Err(e) => return (Vec::new(), Some(format!("Read Codex thread schema: {e}"))),
    };
    let now_ms = unix_time_ms();
    let rows = match stmt.query_map([CODEX_TASK_LIMIT as i64], |row| {
        let id: String = row.get(0)?;
        let updated_at_ms: i64 = row.get(2)?;
        let age_ms = now_ms.saturating_sub(updated_at_ms).max(0);
        let activity = if age_ms <= 120_000 {
            "ACTIVE_RECENTLY"
        } else if age_ms <= 3_600_000 {
            "RECENT"
        } else {
            "IDLE"
        };
        Ok(CodexTaskView {
            display_name: safe_task_label(&id),
            id,
            tokens_used: row.get::<_, i64>(1)?.max(0) as u64,
            updated_at_ms,
            activity: activity.to_string(),
            token_evidence: EvidenceLevel::Measured,
            // SQLite gives a fresh update timestamp, not a running/blocked state.
            activity_evidence: EvidenceLevel::Inferred,
        })
    }) {
        Ok(rows) => rows,
        Err(e) => return (Vec::new(), Some(format!("Query Codex threads: {e}"))),
    };
    let mut tasks = Vec::new();
    for row in rows {
        match row {
            Ok(task) => tasks.push(task),
            Err(e) => return (tasks, Some(format!("Decode Codex thread row: {e}"))),
        }
    }
    (tasks, None)
}

fn local_user_home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

fn update_token_trend(
    tasks: &[CodexTaskView],
    source_available: bool,
    chatgpt_running: bool,
    codex_running: bool,
    session_locked: Option<bool>,
) -> TokenTrendView {
    let sampled_at_ms = unix_time_ms();
    let current: HashMap<String, u64> = tasks
        .iter()
        .map(|task| (task.id.clone(), task.tokens_used))
        .collect();
    let cell = TOKEN_TREND.get_or_init(|| Mutex::new(load_token_trend_state(sampled_at_ms)));
    let mut state = cell.lock().unwrap_or_else(|e| e.into_inner());
    let (interval_secs, delta, tokens_per_min, gap, evidence, lifecycle) = if source_available {
        advance_token_trend_state(
            &mut state,
            current,
            sampled_at_ms,
            chatgpt_running,
            codex_running,
        )
    } else {
        record_missing_token_source(&mut state, sampled_at_ms)
    };
    record_acceptance_lifecycle(&mut state, &lifecycle, sampled_at_ms);
    match session_locked {
        Some(true) => {
            record_acceptance_event(&mut state, "WINDOWS_LOCKED", sampled_at_ms);
        }
        Some(false) => {
            record_acceptance_event(&mut state, "WINDOWS_UNLOCKED", sampled_at_ms);
        }
        None => {}
    }
    persist_token_trend_state(&mut state);
    token_trend_view(
        &state,
        interval_secs,
        delta,
        tokens_per_min,
        gap,
        evidence,
        lifecycle,
        source_available,
        chatgpt_running,
        codex_running,
        sampled_at_ms,
    )
}

fn record_acceptance_lifecycle(state: &mut TokenTrendState, lifecycle: &str, observed_at_ms: i64) {
    let kind = match lifecycle {
        "ACTIVE" => Some("TOKEN_ACTIVE"),
        "IDLE" => Some("TOKEN_IDLE"),
        "APPS_CLOSED" => Some("APPS_CLOSED"),
        "SOURCE_MISSING" => Some("SOURCE_MISSING"),
        "GAP" => Some("SAMPLING_GAP"),
        _ => None,
    };
    if let Some(kind) = kind {
        record_acceptance_event(state, kind, observed_at_ms);
    }
}

fn record_acceptance_event(state: &mut TokenTrendState, kind: &str, observed_at_ms: i64) -> bool {
    if !ACCEPTANCE_EVENT_KINDS.contains(&kind) {
        return false;
    }
    match state.acceptance_events.get_mut(kind) {
        Some(record) if observed_at_ms == record.last_observed_at_ms => false,
        Some(record) => {
            record.first_observed_at_ms = record.first_observed_at_ms.min(observed_at_ms);
            record.last_observed_at_ms = record.last_observed_at_ms.max(observed_at_ms);
            record.observations = record.observations.saturating_add(1);
            true
        }
        None => {
            state.acceptance_events.insert(
                kind.to_string(),
                AcceptanceEventRecord {
                    first_observed_at_ms: observed_at_ms,
                    last_observed_at_ms: observed_at_ms,
                    observations: 1,
                },
            );
            true
        }
    }
}

fn advance_token_trend_state(
    state: &mut TokenTrendState,
    current: HashMap<String, u64>,
    sampled_at_ms: i64,
    chatgpt_running: bool,
    codex_running: bool,
) -> (u64, u64, u64, bool, EvidenceLevel, String) {
    let Some(previous_sampled_at_ms) = state.sampled_at_ms else {
        state.sampled_at_ms = Some(sampled_at_ms);
        state.tokens = current;
        return (
            0,
            0,
            0,
            false,
            EvidenceLevel::MissingProof,
            "BASELINE".to_string(),
        );
    };
    let elapsed_ms = sampled_at_ms.saturating_sub(previous_sampled_at_ms).max(0) as u64;
    let interval_secs = elapsed_ms / 1_000;
    let gap = interval_secs > AI_SAMPLE_GAP_SECS || interval_secs == 0;
    let measured_delta = token_delta(&current, &state.tokens);
    let delta = if gap { 0 } else { measured_delta };
    let tokens_per_min = if !gap && elapsed_ms > 0 {
        (delta as f64 * 60_000.0 / elapsed_ms as f64).round() as u64
    } else {
        0
    };
    let lifecycle = token_lifecycle(gap, delta, chatgpt_running, codex_running).to_string();
    push_token_history(
        &mut state.history,
        TokenTrendPointView {
            sampled_at_ms,
            interval_secs,
            delta_tokens: delta,
            tokens_per_min,
            gap,
            lifecycle: lifecycle.clone(),
            source_available: true,
        },
    );
    state.sampled_at_ms = Some(sampled_at_ms);
    state.tokens = current;
    (
        interval_secs,
        delta,
        tokens_per_min,
        gap,
        if gap {
            EvidenceLevel::MissingProof
        } else {
            // This is derived from two overlapping top-N task snapshots rather
            // than a billing meter or an event stream.
            EvidenceLevel::Inferred
        },
        lifecycle,
    )
}

fn record_missing_token_source(
    state: &mut TokenTrendState,
    attempted_at_ms: i64,
) -> (u64, u64, u64, bool, EvidenceLevel, String) {
    let interval_secs = state
        .sampled_at_ms
        .map(|sampled_at| attempted_at_ms.saturating_sub(sampled_at).max(0) as u64 / 1_000)
        .unwrap_or(0);
    let lifecycle = "SOURCE_MISSING".to_string();
    push_token_history(
        &mut state.history,
        TokenTrendPointView {
            sampled_at_ms: attempted_at_ms,
            interval_secs,
            delta_tokens: 0,
            tokens_per_min: 0,
            gap: true,
            lifecycle: lifecycle.clone(),
            source_available: false,
        },
    );
    (
        interval_secs,
        0,
        0,
        true,
        EvidenceLevel::MissingProof,
        lifecycle,
    )
}

fn token_lifecycle(
    gap: bool,
    delta_tokens: u64,
    chatgpt_running: bool,
    codex_running: bool,
) -> &'static str {
    if gap {
        "GAP"
    } else if delta_tokens > 0 {
        "ACTIVE"
    } else if !chatgpt_running && !codex_running {
        "APPS_CLOSED"
    } else {
        "IDLE"
    }
}

fn token_trend_view(
    state: &TokenTrendState,
    interval_secs: u64,
    delta_tokens: u64,
    tokens_per_min: u64,
    gap: bool,
    evidence: EvidenceLevel,
    lifecycle: String,
    source_available: bool,
    chatgpt_running: bool,
    codex_running: bool,
    attempted_at_ms: i64,
) -> TokenTrendView {
    TokenTrendView {
        interval_secs,
        delta_tokens,
        tokens_per_min,
        gap,
        evidence,
        lifecycle,
        source_available,
        chatgpt_running,
        codex_running,
        history: state.history.iter().cloned().collect(),
        history_persisted: state.persistence_ok,
        history_restored: state.history_restored,
        persistence_error: state.persistence_error.clone(),
        last_sampled_at_ms: state.sampled_at_ms,
        last_attempted_at_ms: Some(attempted_at_ms),
    }
}

fn token_trend_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("SIRIN_AI_MONITOR_TREND_PATH") {
        if !path.is_empty() {
            return std::path::PathBuf::from(path);
        }
    }
    crate::platform::app_data_dir()
        .join("tracking")
        .join(TOKEN_TREND_FILE)
}

fn load_token_trend_state(now_ms: i64) -> TokenTrendState {
    match load_token_trend_state_from_path(&token_trend_path(), now_ms) {
        Ok(Some(state)) => state,
        Ok(None) => TokenTrendState::default(),
        Err(error) => TokenTrendState {
            persistence_error: Some(error),
            ..TokenTrendState::default()
        },
    }
}

fn load_token_trend_state_from_path(
    path: &std::path::Path,
    now_ms: i64,
) -> Result<Option<TokenTrendState>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Read persisted token trend: {error}")),
    };
    let line = raw
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| "Persisted token trend is empty".to_string())?;
    let mut persisted: PersistedTokenTrend = serde_json::from_str(line)
        .map_err(|error| format!("Decode persisted token trend: {error}"))?;
    if persisted.schema_version != TOKEN_TREND_SCHEMA_VERSION {
        return Err(format!(
            "Unsupported persisted token trend schema: {}",
            persisted.schema_version
        ));
    }

    let earliest_ms = now_ms.saturating_sub(TOKEN_HISTORY_WINDOW_MS);
    persisted
        .history
        .retain(|point| point.sampled_at_ms >= earliest_ms && point.sampled_at_ms <= now_ms);
    for point in &mut persisted.history {
        if point.lifecycle.is_empty() {
            point.lifecycle = if point.gap { "GAP" } else { "LEGACY" }.to_string();
        }
    }
    while persisted.history.len() > TOKEN_HISTORY_LIMIT {
        persisted.history.pop_front();
    }
    let baseline_is_fresh = persisted
        .sampled_at_ms
        .is_some_and(|sampled_at| sampled_at >= earliest_ms && sampled_at <= now_ms);
    persisted
        .acceptance_events
        .retain(|kind, _| ACCEPTANCE_EVENT_KINDS.contains(&kind.as_str()));
    let mut state = TokenTrendState {
        sampled_at_ms: baseline_is_fresh
            .then_some(persisted.sampled_at_ms)
            .flatten(),
        tokens: if baseline_is_fresh {
            persisted.tokens
        } else {
            HashMap::new()
        },
        history: persisted.history,
        acceptance_events: persisted.acceptance_events,
        history_restored: true,
        persistence_ok: true,
        persistence_error: None,
    };
    // Schema v1 files written before the acceptance ledger are upgraded from
    // their still-bounded recent history. Existing ledgers are left intact so
    // process restarts do not double-count the same minute samples.
    if state.acceptance_events.is_empty() {
        let recent = state
            .history
            .iter()
            .map(|point| (point.lifecycle.clone(), point.sampled_at_ms))
            .collect::<Vec<_>>();
        for (lifecycle, observed_at_ms) in recent {
            record_acceptance_lifecycle(&mut state, &lifecycle, observed_at_ms);
        }
    }
    record_acceptance_event(&mut state, "HISTORY_RESTORED", now_ms);
    Ok(Some(state))
}

fn persist_token_trend_state(state: &mut TokenTrendState) {
    match persist_token_trend_state_to_path(state, &token_trend_path()) {
        Ok(()) => {
            TOKEN_TREND_WRITES.fetch_add(1, Ordering::Relaxed);
            state.persistence_ok = true;
            state.persistence_error = None;
        }
        Err(error) => {
            state.persistence_ok = false;
            state.persistence_error = Some(error.clone());
            tracing::warn!(target: "sirin", "[ai-monitor] token trend persistence failed: {error}");
        }
    }
}

fn persist_token_trend_state_to_path(
    state: &TokenTrendState,
    path: &std::path::Path,
) -> Result<(), String> {
    let persisted = PersistedTokenTrend {
        schema_version: TOKEN_TREND_SCHEMA_VERSION,
        sampled_at_ms: state.sampled_at_ms,
        tokens: state.tokens.clone(),
        history: state.history.clone(),
        acceptance_events: state.acceptance_events.clone(),
    };
    atomic_write_token_trend(path, &persisted)
}

fn atomic_write_token_trend(
    path: &std::path::Path,
    persisted: &PersistedTokenTrend,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Create token trend directory: {error}"))?;
    }
    let sequence = TOKEN_TREND_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(TOKEN_TREND_FILE);
    let temp_path = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|error| format!("Create token trend snapshot: {error}"))?;
        serde_json::to_writer(&mut file, persisted)
            .map_err(|error| format!("Encode token trend snapshot: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("Finish token trend snapshot: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Sync token trend snapshot: {error}"))?;
        drop(file);
        std::fs::rename(&temp_path, path)
            .map_err(|error| format!("Replace token trend snapshot: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn token_delta(current: &HashMap<String, u64>, previous: &HashMap<String, u64>) -> u64 {
    current
        .iter()
        .filter_map(|(id, tokens)| previous.get(id).map(|old| tokens.saturating_sub(*old)))
        .sum()
}

fn push_token_history(history: &mut VecDeque<TokenTrendPointView>, point: TokenTrendPointView) {
    if history.len() == TOKEN_HISTORY_LIMIT {
        history.pop_front();
    }
    history.push_back(point);
}

fn collect_processes() -> (Vec<ProcessGroupView>, Option<String>) {
    #[cfg(not(windows))]
    {
        return (
            default_process_groups(EvidenceLevel::MissingProof),
            Some("Process monitoring is currently Windows-only".into()),
        );
    }

    #[cfg(windows)]
    {
        let output = match run_utf8_command("tasklist.exe /FO CSV /NH") {
            Ok(output) => output,
            Err(e) => return (default_process_groups(EvidenceLevel::MissingProof), Some(e)),
        };
        let mut groups: HashMap<&'static str, (u32, u64)> =
            HashMap::from([("ChatGPT", (0, 0)), ("Codex", (0, 0)), ("Sirin", (0, 0))]);
        for line in output.lines() {
            let fields = parse_csv_line(line);
            if fields.len() < 5 {
                continue;
            }
            let name = fields[0].to_ascii_lowercase();
            let group = if name == "chatgpt.exe" {
                Some("ChatGPT")
            } else if name.starts_with("codex") && name.ends_with(".exe") {
                Some("Codex")
            } else if name == "sirin.exe" {
                Some("Sirin")
            } else {
                None
            };
            let Some(group) = group else { continue };
            let kb = fields[4]
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .parse::<u64>()
                .unwrap_or(0);
            let entry = groups.entry(group).or_default();
            entry.0 = entry.0.saturating_add(1);
            entry.1 = entry.1.saturating_add(kb);
        }
        let order = ["ChatGPT", "Codex", "Sirin"];
        let result = order
            .into_iter()
            .map(|app| {
                let (count, kb) = groups.get(app).copied().unwrap_or_default();
                ProcessGroupView {
                    app: app.to_string(),
                    running: count > 0,
                    process_count: count,
                    working_set_mb: kb as f64 / 1024.0,
                    evidence: EvidenceLevel::Measured,
                }
            })
            .collect();
        (result, None)
    }
}

fn default_process_groups(evidence: EvidenceLevel) -> Vec<ProcessGroupView> {
    ["ChatGPT", "Codex", "Sirin"]
        .into_iter()
        .map(|app| ProcessGroupView {
            app: app.to_string(),
            evidence,
            ..Default::default()
        })
        .collect()
}

fn collect_network() -> NetworkSnapshot {
    #[cfg(not(windows))]
    {
        return NetworkSnapshot {
            evidence: EvidenceLevel::MissingProof,
            source_error: Some("Network route monitoring is currently Windows-only".into()),
            ..Default::default()
        };
    }

    #[cfg(windows)]
    {
        let (ipv4_interfaces_text, ipv6_interfaces_text, ipv4_text, ipv6_text, wifi_text) =
            thread::scope(|scope| {
                let i4 =
                    scope.spawn(|| run_utf8_command("netsh.exe interface ipv4 show interfaces"));
                let i6 =
                    scope.spawn(|| run_utf8_command("netsh.exe interface ipv6 show interfaces"));
                let r4 = scope
                    .spawn(|| run_utf8_command("netsh.exe interface ipv4 show route store=active"));
                let r6 = scope
                    .spawn(|| run_utf8_command("netsh.exe interface ipv6 show route store=active"));
                let wifi = scope.spawn(|| run_utf8_command("netsh.exe wlan show interfaces"));
                (
                    i4.join()
                        .unwrap_or_else(|_| Err("IPv4 interface collector panicked".to_string())),
                    i6.join()
                        .unwrap_or_else(|_| Err("IPv6 interface collector panicked".to_string())),
                    r4.join()
                        .unwrap_or_else(|_| Err("IPv4 route collector panicked".to_string())),
                    r6.join()
                        .unwrap_or_else(|_| Err("IPv6 route collector panicked".to_string())),
                    wifi.join()
                        .unwrap_or_else(|_| Err("Wi-Fi collector panicked".to_string())),
                )
            });

        let (ipv4_interfaces_text, ipv6_interfaces_text, ipv4_text, ipv6_text) = match (
            ipv4_interfaces_text,
            ipv6_interfaces_text,
            ipv4_text,
            ipv6_text,
        ) {
            (Ok(i4), Ok(i6), Ok(v4), Ok(v6)) => (i4, i6, v4, v6),
            (i4, i6, v4, v6) => {
                let errors = [i4.err(), i6.err(), v4.err(), v6.err()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("; ");
                return NetworkSnapshot {
                    evidence: EvidenceLevel::MissingProof,
                    wifi: wifi_text.ok().and_then(|text| parse_wifi(&text)),
                    source_error: Some(errors),
                    ..Default::default()
                };
            }
        };

        let mut ipv4_interfaces = parse_interfaces(&ipv4_interfaces_text, "IPv4");
        let mut ipv6_interfaces = parse_interfaces(&ipv6_interfaces_text, "IPv6");
        let ipv4_metrics: HashMap<u32, u32> = ipv4_interfaces
            .iter()
            .map(|iface| (iface.index, iface.metric))
            .collect();
        let ipv6_metrics: HashMap<u32, u32> = ipv6_interfaces
            .iter()
            .map(|iface| (iface.index, iface.metric))
            .collect();
        let ipv4_aliases: HashMap<u32, String> = ipv4_interfaces
            .iter()
            .map(|iface| (iface.index, iface.alias.clone()))
            .collect();
        let ipv6_aliases: HashMap<u32, String> = ipv6_interfaces
            .iter()
            .map(|iface| (iface.index, iface.alias.clone()))
            .collect();
        let mut routes = parse_default_routes(&ipv4_text, "IPv4", &ipv4_metrics, &ipv4_aliases);
        routes.extend(parse_default_routes(
            &ipv6_text,
            "IPv6",
            &ipv6_metrics,
            &ipv6_aliases,
        ));
        routes.sort_by_key(|route| (route.family.clone(), route.effective_metric));

        let (ipv4_latency, ipv6_latency) = thread::scope(|scope| {
            let v4 = scope.spawn(|| ping_latency("IPv4", "1.1.1.1"));
            let v6 = scope.spawn(|| ping_latency("IPv6", "2606:4700:4700::1111"));
            (v4.join().unwrap_or(None), v6.join().unwrap_or(None))
        });
        for (family, target, measured_latency) in [
            ("IPv4", "1.1.1.1", ipv4_latency),
            ("IPv6", "2606:4700:4700::1111", ipv6_latency),
        ] {
            if let Some(route) = routes.iter_mut().find(|route| route.family == family) {
                route.selected = true;
                route.latency_target = Some(target.to_string());
                match measured_latency {
                    Some(ms) => {
                        route.latency_ms = Some(ms);
                        route.latency_status = match ms {
                            0..=80 => "GOOD",
                            81..=180 => "DEGRADED",
                            _ => "SLOW",
                        }
                        .to_string();
                        route.latency_evidence = EvidenceLevel::Measured;
                    }
                    None => {
                        route.latency_status = "NO_REPLY".to_string();
                        route.latency_evidence = EvidenceLevel::MissingProof;
                    }
                }
            }
        }

        let selected_v4 = routes
            .iter()
            .find(|route| route.family == "IPv4" && route.selected);
        let selected_v6 = routes
            .iter()
            .find(|route| route.family == "IPv6" && route.selected);
        for iface in &mut ipv4_interfaces {
            iface.active_ipv4_default =
                selected_v4.is_some_and(|route| route.interface_index == iface.index);
        }
        for iface in &mut ipv6_interfaces {
            iface.active_ipv6_default =
                selected_v6.is_some_and(|route| route.interface_index == iface.index);
        }
        let mut interfaces = ipv4_interfaces;
        interfaces.extend(ipv6_interfaces);
        interfaces.sort_by_key(|iface| (iface.family.clone(), iface.metric, iface.index));
        let split_default_route = match (selected_v4, selected_v6) {
            (Some(v4), Some(v6)) => v4.interface_index != v6.interface_index,
            _ => false,
        };
        let mut warnings = Vec::new();
        if split_default_route {
            warnings.push("IPv4 與 IPv6 的預設路由正在使用不同介面".to_string());
        }
        for route in routes.iter().filter(|route| route.selected) {
            if route.latency_status == "NO_REPLY" {
                warnings.push(format!(
                    "{} 預設路由存在，但低成本延遲探測沒有回覆",
                    route.family
                ));
            }
        }
        NetworkSnapshot {
            evidence: EvidenceLevel::Measured,
            default_routes: routes,
            interfaces,
            wifi: wifi_text.ok().and_then(|text| parse_wifi(&text)),
            split_default_route,
            warnings,
            source_error: None,
        }
    }
}

#[cfg(windows)]
fn run_utf8_command(command: &str) -> Result<String, String> {
    let output = Command::new("cmd.exe")
        .no_window()
        .args(["/D", "/S", "/C", &format!("chcp 65001>nul & {command}")])
        .output()
        .map_err(|e| format!("Run {command}: {e}"))?;
    if !output.status.success() {
        return Err(format!("{command} exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
fn run_program_utf8(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .no_window()
        .args(args)
        .output()
        .map_err(|error| format!("Run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_interfaces(text: &str, family: &str) -> Vec<InterfaceView> {
    let mut interfaces = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let (Ok(index), Ok(metric)) = (fields[0].parse::<u32>(), fields[1].parse::<u32>()) else {
            continue;
        };
        interfaces.push(InterfaceView {
            family: family.to_string(),
            index,
            alias: fields[4..].join(" "),
            state: fields[3].to_string(),
            metric,
            active_ipv4_default: false,
            active_ipv6_default: false,
            evidence: EvidenceLevel::Measured,
        });
    }
    interfaces.sort_by_key(|iface| iface.metric);
    interfaces
}

fn parse_default_routes(
    text: &str,
    family: &str,
    metrics: &HashMap<u32, u32>,
    aliases: &HashMap<u32, String>,
) -> Vec<DefaultRouteView> {
    let prefix = if family == "IPv4" {
        "0.0.0.0/0"
    } else {
        "::/0"
    };
    let mut routes = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 || fields[3] != prefix {
            continue;
        }
        let (Ok(route_metric), Ok(interface_index)) =
            (fields[2].parse::<u32>(), fields[4].parse::<u32>())
        else {
            continue;
        };
        let interface_metric = metrics.get(&interface_index).copied().unwrap_or(0);
        routes.push(DefaultRouteView {
            family: family.to_string(),
            interface_index,
            interface_alias: aliases
                .get(&interface_index)
                .cloned()
                .unwrap_or_else(|| format!("Interface {interface_index}")),
            gateway: fields[5..].join(" "),
            route_metric,
            interface_metric,
            effective_metric: route_metric.saturating_add(interface_metric),
            selected: false,
            latency_status: "NOT_TESTED".to_string(),
            latency_evidence: EvidenceLevel::MissingProof,
            ..Default::default()
        });
    }
    routes.sort_by_key(|route| route.effective_metric);
    routes
}

fn parse_wifi(text: &str) -> Option<WifiView> {
    let mut values = HashMap::<String, String>::new();
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        values.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    if values.is_empty() {
        return None;
    }
    let signal_percent = values
        .get("signal")
        .and_then(|value| value.trim_end_matches('%').trim().parse::<u32>().ok());
    // Windows netsh exposes signal quality but not RSSI. The conventional
    // quality-to-RSSI approximation is deliberately labelled INFERRED.
    let rssi_dbm_approx = signal_percent.map(|value| -100 + (value.min(100) as i32 / 2));
    let number = |key: &str| values.get(key).and_then(|value| value.parse::<f64>().ok());
    Some(WifiView {
        state: values.get("state").cloned(),
        ssid: values.get("ssid").cloned(),
        signal_percent,
        rssi_dbm_approx,
        rssi_evidence: if rssi_dbm_approx.is_some() {
            EvidenceLevel::Inferred
        } else {
            EvidenceLevel::MissingProof
        },
        receive_rate_mbps: number("receive rate (mbps)"),
        transmit_rate_mbps: number("transmit rate (mbps)"),
        radio_type: values.get("radio type").cloned(),
        channel: values.get("channel").and_then(|value| value.parse().ok()),
        evidence: EvidenceLevel::Measured,
    })
}

#[cfg(windows)]
fn ping_latency(family: &str, target: &str) -> Option<u32> {
    let flag = if family == "IPv6" { "-6" } else { "-4" };
    let command = format!("ping.exe {flag} -n 1 -w 1200 {target}");
    let output = run_utf8_command(&command).ok()?;
    parse_ping_latency(&output)
}

#[cfg(not(windows))]
fn ping_latency(_family: &str, _target: &str) -> Option<u32> {
    None
}

fn parse_ping_latency(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    for marker in ["time=", "time<", "時間=", "時間<"] {
        if let Some(pos) = lower.find(marker) {
            let rest = &lower[pos + marker.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(value) = digits.parse::<u32>() {
                return Some(value.max(1));
            }
        }
    }
    None
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}

fn safe_task_label(id: &str) -> String {
    let short = id.chars().take(8).collect::<String>();
    if short.is_empty() {
        "Codex 工作".to_string()
    } else {
        format!("Codex 工作 {short}")
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interface_table_and_default_routes() {
        let interfaces_text = "  7  11  1500  connected  Wi-Fi 2\n 11 601 10240 connected 行動電話";
        let interfaces = parse_interfaces(interfaces_text, "IPv4");
        assert_eq!(interfaces.len(), 2);
        assert_eq!(interfaces[0].alias, "Wi-Fi 2");
        assert_eq!(interfaces[0].family, "IPv4");
        let metrics = interfaces.iter().map(|i| (i.index, i.metric)).collect();
        let aliases = interfaces
            .iter()
            .map(|i| (i.index, i.alias.clone()))
            .collect();
        let routes = parse_default_routes(
            "No Manual 786 0.0.0.0/0 7 192.168.1.1\nNo Manual 256 0.0.0.0/0 11 10.100.1.37",
            "IPv4",
            &metrics,
            &aliases,
        );
        assert_eq!(routes[0].interface_alias, "Wi-Fi 2");
        assert_eq!(routes[0].effective_metric, 797);
    }

    #[test]
    fn parses_wifi_and_labels_rssi_as_inferred() {
        let wifi = parse_wifi(
            "State : connected\nSSID : FloraHouse\nSignal : 86%\nReceive rate (Mbps) : 866.7\nTransmit rate (Mbps) : 780\nRadio type : 802.11ac\nChannel : 44",
        )
        .unwrap();
        assert_eq!(wifi.signal_percent, Some(86));
        assert_eq!(wifi.rssi_dbm_approx, Some(-57));
        assert_eq!(wifi.rssi_evidence, EvidenceLevel::Inferred);
        assert_eq!(wifi.receive_rate_mbps, Some(866.7));
    }

    #[test]
    fn parses_tasklist_csv_with_memory_commas() {
        let fields = parse_csv_line("\"ChatGPT.exe\",\"10188\",\"Console\",\"1\",\"331,104 K\"");
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[0], "ChatGPT.exe");
        assert_eq!(fields[4], "331,104 K");
    }

    #[test]
    fn parses_ping_latency() {
        assert_eq!(parse_ping_latency("Reply: time=60ms TTL=55"), Some(60));
        assert_eq!(parse_ping_latency("Reply: time<1ms TTL=128"), Some(1));
    }

    #[test]
    fn builds_non_sensitive_task_label_from_id_only() {
        assert_eq!(safe_task_label("01a024cf-729c"), "Codex 工作 01a024cf");
        assert_eq!(safe_task_label(""), "Codex 工作");
    }

    #[test]
    fn token_delta_counts_only_growth_for_overlapping_tasks() {
        let previous = HashMap::from([("a".to_string(), 100_u64), ("b".to_string(), 200_u64)]);
        let current = HashMap::from([
            ("a".to_string(), 160_u64),
            ("b".to_string(), 150_u64),
            ("new".to_string(), 999_u64),
        ]);
        assert_eq!(token_delta(&current, &previous), 60);
    }

    #[test]
    fn token_history_keeps_latest_hour_of_points() {
        let mut history = VecDeque::new();
        for value in 0_u64..65 {
            push_token_history(
                &mut history,
                TokenTrendPointView {
                    sampled_at_ms: (value * 60_000) as i64,
                    interval_secs: 60,
                    delta_tokens: value,
                    tokens_per_min: value,
                    gap: false,
                    lifecycle: "ACTIVE".to_string(),
                    source_available: true,
                },
            );
        }
        assert_eq!(history.len(), TOKEN_HISTORY_LIMIT);
        assert_eq!(history.front().map(|point| point.tokens_per_min), Some(5));
        assert_eq!(history.back().map(|point| point.tokens_per_min), Some(64));
    }

    #[test]
    fn token_trend_gap_rebaselines_without_inventing_a_rate() {
        let mut state = TokenTrendState::default();
        let first = HashMap::from([("task-a".to_string(), 100_u64)]);
        let second = HashMap::from([("task-a".to_string(), 160_u64)]);
        let after_gap = HashMap::from([("task-a".to_string(), 9_000_u64)]);

        assert_eq!(
            advance_token_trend_state(&mut state, first, 1_000, true, true),
            (
                0,
                0,
                0,
                false,
                EvidenceLevel::MissingProof,
                "BASELINE".to_string()
            )
        );
        let normal = advance_token_trend_state(&mut state, second, 61_000, true, true);
        assert_eq!(
            normal,
            (
                60,
                60,
                60,
                false,
                EvidenceLevel::Inferred,
                "ACTIVE".to_string()
            )
        );

        let interrupted = advance_token_trend_state(&mut state, after_gap, 161_001, true, true);
        assert_eq!(
            interrupted,
            (
                100,
                0,
                0,
                true,
                EvidenceLevel::MissingProof,
                "GAP".to_string()
            )
        );
        let last = state.history.back().expect("gap point");
        assert!(last.gap);
        assert_eq!(last.delta_tokens, 0);
        assert_eq!(last.tokens_per_min, 0);
        assert_eq!(last.lifecycle, "GAP");
    }

    #[test]
    fn token_trend_distinguishes_closed_apps_from_missing_source() {
        let mut state = TokenTrendState::default();
        let baseline = HashMap::from([("task-a".to_string(), 100_u64)]);
        let unchanged = baseline.clone();
        let resumed = HashMap::from([("task-a".to_string(), 500_u64)]);

        let _ = advance_token_trend_state(&mut state, baseline, 1_000, true, true);
        let closed = advance_token_trend_state(&mut state, unchanged, 61_000, false, false);
        assert_eq!(closed.5, "APPS_CLOSED");
        assert_eq!(closed.1, 0);
        assert_eq!(state.sampled_at_ms, Some(61_000));

        let missing = record_missing_token_source(&mut state, 121_000);
        assert_eq!(missing.5, "SOURCE_MISSING");
        assert_eq!(state.sampled_at_ms, Some(61_000));
        assert!(
            !state
                .history
                .back()
                .expect("missing point")
                .source_available
        );

        let after_missing = advance_token_trend_state(&mut state, resumed, 181_001, true, true);
        assert_eq!(after_missing.5, "GAP");
        assert_eq!(after_missing.1, 0);
    }

    #[test]
    fn acceptance_classifier_requires_real_cycles_and_keeps_optional_source_separate() {
        let mut state = TokenTrendState::default();
        for (kind, observed_at_ms) in [
            ("TOKEN_ACTIVE", 100),
            ("TOKEN_IDLE", 200),
            ("APPS_CLOSED", 300),
            ("TOKEN_ACTIVE", 400),
            ("SOURCE_MISSING", 500),
            ("SAMPLING_GAP", 600),
            ("WINDOWS_LOCKED", 700),
            ("WINDOWS_UNLOCKED", 800),
            ("HISTORY_RESTORED", 900),
            ("STANDBY_CYCLE", 1_000),
        ] {
            assert!(record_acceptance_event(&mut state, kind, observed_at_ms));
        }
        assert!(!record_acceptance_event(
            &mut state,
            "NOT_A_REAL_EVENT",
            2_000
        ));
        let acceptance = classify_monitor_acceptance(
            &state.acceptance_events,
            true,
            true,
            &PowerSnapshot::default(),
        );
        assert_eq!(acceptance.status, "PASS");
        assert!(acceptance.missing_required_modes.is_empty());
        assert!(acceptance.ledger_persisted);
        assert!(acceptance.ledger_restored);
        assert_eq!(acceptance.modes.len(), 6);
        assert_eq!(
            acceptance
                .modes
                .iter()
                .find(|mode| mode.mode == "TOKEN_SOURCE_RECOVERY")
                .map(|mode| (mode.required, mode.status.as_str())),
            Some((false, "PASS"))
        );
    }

    #[test]
    fn standby_acceptance_requires_gap_near_the_resume_event() {
        let mut history = VecDeque::new();
        push_token_history(
            &mut history,
            TokenTrendPointView {
                sampled_at_ms: 250_000,
                interval_secs: 150,
                delta_tokens: 0,
                tokens_per_min: 0,
                gap: true,
                lifecycle: "GAP".to_string(),
                source_available: true,
            },
        );
        assert!(standby_gap_correlated(&history, 100_000, 220_000));
        assert!(!standby_gap_correlated(&history, 1_000, 10_000));
        assert!(!standby_gap_correlated(&history, 300_000, 200_000));
    }

    #[test]
    fn parses_modern_standby_enter_and_resume_events() {
        let xml = r#"
            <Events>
              <Event><System><Provider Name="Microsoft-Windows-Kernel-Power"/><EventID>507</EventID><TimeCreated SystemTime="2026-08-26T01:21:11.6872336Z"/></System></Event>
              <Event><System><Provider Name="Microsoft-Windows-Kernel-Power"/><EventID>506</EventID><TimeCreated SystemTime="2026-08-26T01:19:33.1428631Z"/></System></Event>
              <Event><System><EventID>42</EventID><TimeCreated SystemTime="2026-08-26T01:00:00Z"/></System></Event>
            </Events>
        "#;
        let events = parse_power_events(xml);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, 507);
        assert_eq!(events[0].timestamp, "2026-08-26T01:21:11Z");
        assert_eq!(events[1].id, 506);
        assert_eq!(events[1].timestamp, "2026-08-26T01:19:33Z");
        assert!(events[0].timestamp_ms > events[1].timestamp_ms);
    }

    #[test]
    fn local_alerts_explain_lock_guard_and_stale_sample_actions() {
        let ai_work = AiWorkSnapshot::default();
        let power = PowerSnapshot {
            session_locked: Some(true),
            awake_guard: AwakeGuardView {
                enabled: true,
                chatgpt_running: true,
                request_active: false,
                evidence: EvidenceLevel::MissingProof,
                ..Default::default()
            },
            ..Default::default()
        };
        let recovery = RecoverySnapshot {
            sample_age_secs: Some(AI_SAMPLE_GAP_SECS + 1),
            evidence: EvidenceLevel::Measured,
            ..Default::default()
        };

        let alerts = build_local_alerts(
            &ai_work,
            &CodexHealthView::default(),
            &power,
            &recovery,
            &MonitorOverheadSnapshot::default(),
        );
        let codes = alerts
            .iter()
            .map(|alert| alert.code.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            codes,
            [
                "WINDOWS_SESSION_LOCKED",
                "AWAKE_GUARD_FAILED",
                "AI_SAMPLE_STALE"
            ]
        );
        assert!(alerts.iter().all(|alert| !alert.action.is_empty()));
    }

    #[test]
    fn local_alerts_enforce_monitor_overhead_budgets_without_cleanup() {
        let overhead = MonitorOverheadSnapshot {
            evidence: EvidenceLevel::Measured,
            last_sampler_wall_ms: 5_001.0,
            last_sampler_cpu_ms: Some(501.0),
            last_network_collection_ms: 5_001.0,
            trend_file_bytes: Some(1_048_577),
            ..Default::default()
        };
        let alerts = build_local_alerts(
            &AiWorkSnapshot::default(),
            &CodexHealthView::default(),
            &PowerSnapshot::default(),
            &RecoverySnapshot::default(),
            &overhead,
        );
        assert_eq!(
            alerts
                .iter()
                .map(|alert| alert.code.as_str())
                .collect::<Vec<_>>(),
            [
                "MONITOR_SAMPLER_SLOW",
                "MONITOR_NETWORK_COLLECTION_SLOW",
                "TOKEN_TREND_FILE_TOO_LARGE"
            ]
        );
        assert!(alerts
            .iter()
            .all(|alert| !alert.action.contains("自動刪除") || alert.action.contains("不要")));
    }

    fn healthy_system_resources() -> SystemResourceView {
        SystemResourceView {
            evidence: EvidenceLevel::Measured,
            cpu_percent_recent: Some(42.0),
            cpu_interval_secs: Some(60.0),
            cpu_evidence: EvidenceLevel::Measured,
            physical_memory_total_gb: Some(48.0),
            physical_memory_available_gb: Some(24.0),
            physical_memory_available_percent: Some(50.0),
            committed_percent: Some(67.0),
            memory_evidence: EvidenceLevel::Measured,
            system_drive: Some("C:".to_string()),
            system_drive_free_gb: Some(78.0),
            system_drive_free_percent: Some(26.0),
            disk_evidence: EvidenceLevel::Measured,
            ai_working_set_mb: 2_950.0,
            pressure: "HEALTHY".to_string(),
            ..Default::default()
        }
    }

    fn good_ipv4_network() -> NetworkSnapshot {
        NetworkSnapshot {
            evidence: EvidenceLevel::Measured,
            default_routes: vec![DefaultRouteView {
                family: "IPv4".to_string(),
                interface_alias: "Wi-Fi 2".to_string(),
                selected: true,
                latency_ms: Some(15),
                latency_status: "GOOD".to_string(),
                latency_evidence: EvidenceLevel::Measured,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn resource_pressure_uses_sustained_cpu_memory_and_disk_thresholds() {
        let healthy = healthy_system_resources();
        assert_eq!(classify_system_pressure(&healthy).0, "HEALTHY");

        let warning = SystemResourceView {
            cpu_percent_recent: Some(91.0),
            ..healthy.clone()
        };
        let (status, reasons) = classify_system_pressure(&warning);
        assert_eq!(status, "WARNING");
        assert!(reasons.iter().any(|reason| reason.contains("CPU")));

        let critical = SystemResourceView {
            physical_memory_available_percent: Some(7.0),
            ..healthy
        };
        assert_eq!(classify_system_pressure(&critical).0, "CRITICAL");
    }

    #[test]
    fn codex_health_reports_progress_without_claiming_remote_limit_proof() {
        let now = unix_time_ms();
        let ai_work = AiWorkSnapshot {
            processes: vec![ProcessGroupView {
                app: "Codex".to_string(),
                running: true,
                process_count: 2,
                working_set_mb: 760.0,
                evidence: EvidenceLevel::Measured,
            }],
            codex_tasks: vec![CodexTaskView {
                id: "opaque".to_string(),
                display_name: "Codex 工作 opaque".to_string(),
                tokens_used: 10_000,
                updated_at_ms: now - 15_000,
                activity: "ACTIVE_RECENTLY".to_string(),
                token_evidence: EvidenceLevel::Measured,
                activity_evidence: EvidenceLevel::Inferred,
            }],
            codex_token_trend: TokenTrendView {
                delta_tokens: 500,
                evidence: EvidenceLevel::Inferred,
                ..Default::default()
            },
            system_resources: healthy_system_resources(),
            ..Default::default()
        };
        let health = build_codex_health(&ai_work, &good_ipv4_network());
        assert_eq!(health.status, "HEALTHY_ACTIVITY");
        assert_eq!(health.progress_status, "PROGRESS_OBSERVED");
        assert_eq!(health.local_resource_status, "HEALTHY");
        assert_eq!(health.remote_limit_status, "MISSING_PROOF");
        assert_eq!(health.remote_limit_evidence, EvidenceLevel::MissingProof);
    }

    #[test]
    fn codex_health_does_not_call_absence_of_updates_a_stall() {
        let ai_work = AiWorkSnapshot {
            processes: vec![ProcessGroupView {
                app: "Codex".to_string(),
                running: true,
                process_count: 1,
                evidence: EvidenceLevel::Measured,
                ..Default::default()
            }],
            system_resources: healthy_system_resources(),
            ..Default::default()
        };
        let health = build_codex_health(&ai_work, &good_ipv4_network());
        assert_eq!(health.status, "WAITING_OR_IDLE");
        assert_eq!(health.progress_status, "SUSPECTED_STALL_OR_WAITING");
        assert_eq!(health.evidence, EvidenceLevel::MissingProof);
        assert!(health.detail.contains("無法區分"));
    }

    #[test]
    fn codex_health_prioritizes_local_resource_pressure() {
        let mut resources = healthy_system_resources();
        resources.physical_memory_available_percent = Some(6.0);
        let (pressure, reasons) = classify_system_pressure(&resources);
        resources.pressure = pressure;
        resources.pressure_reasons = reasons;
        let ai_work = AiWorkSnapshot {
            processes: vec![ProcessGroupView {
                app: "Codex".to_string(),
                running: true,
                process_count: 1,
                evidence: EvidenceLevel::Measured,
                ..Default::default()
            }],
            system_resources: resources,
            ..Default::default()
        };
        let health = build_codex_health(&ai_work, &good_ipv4_network());
        assert_eq!(health.status, "LOCAL_RESOURCE_PRESSURE");
        assert_eq!(health.severity, "CRITICAL");
    }

    #[test]
    fn persisted_token_trend_round_trips_and_expires_old_data() {
        let path = std::env::temp_dir().join(format!(
            "sirin-ai-monitor-token-trend-{}-{}.jsonl",
            std::process::id(),
            unix_time_ms()
        ));
        let mut state = TokenTrendState {
            sampled_at_ms: Some(120_000),
            tokens: HashMap::from([("opaque-task-id".to_string(), 321_u64)]),
            ..TokenTrendState::default()
        };
        push_token_history(
            &mut state.history,
            TokenTrendPointView {
                sampled_at_ms: 120_000,
                interval_secs: 60,
                delta_tokens: 21,
                tokens_per_min: 21,
                gap: false,
                lifecycle: "ACTIVE".to_string(),
                source_available: true,
            },
        );
        record_acceptance_event(&mut state, "TOKEN_ACTIVE", 120_000);

        persist_token_trend_state_to_path(&state, &path).expect("persist trend");
        let restored = load_token_trend_state_from_path(&path, 180_000)
            .expect("load trend")
            .expect("trend exists");
        assert!(restored.history_restored);
        assert!(restored.persistence_ok);
        assert_eq!(restored.sampled_at_ms, Some(120_000));
        assert_eq!(restored.tokens.get("opaque-task-id"), Some(&321));
        assert_eq!(restored.history.len(), 1);
        assert!(restored.acceptance_events.contains_key("TOKEN_ACTIVE"));
        assert!(restored.acceptance_events.contains_key("HISTORY_RESTORED"));

        let expired =
            load_token_trend_state_from_path(&path, 120_000 + TOKEN_HISTORY_WINDOW_MS + 1)
                .expect("load expired trend")
                .expect("trend exists");
        assert_eq!(expired.sampled_at_ms, None);
        assert!(expired.tokens.is_empty());
        assert!(expired.history.is_empty());
        assert!(expired.acceptance_events.contains_key("TOKEN_ACTIVE"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_schema_without_acceptance_ledger_still_loads() {
        let path = std::env::temp_dir().join(format!(
            "sirin-ai-monitor-token-trend-legacy-{}-{}.jsonl",
            std::process::id(),
            unix_time_ms()
        ));
        std::fs::write(
            &path,
            r#"{"schema_version":1,"sampled_at_ms":120000,"tokens":{},"history":[]}"#,
        )
        .expect("write legacy trend");
        let restored = load_token_trend_state_from_path(&path, 180_000)
            .expect("load legacy trend")
            .expect("legacy trend exists");
        assert!(restored.history_restored);
        assert!(restored.acceptance_events.contains_key("HISTORY_RESTORED"));
        let _ = std::fs::remove_file(path);
    }

    /// Manual acceptance for the owned Windows workstation. Kept ignored so
    /// normal CI remains hermetic and never probes a developer's network or
    /// Codex state database without an explicit command.
    #[cfg(windows)]
    #[test]
    #[ignore = "manual Windows acceptance"]
    fn live_snapshot_is_read_only_and_has_direct_evidence() {
        let live = snapshot();
        assert!(live.safety.local_only);
        assert!(live.safety.read_only);
        assert!(!live.safety.credentials_accessed);
        assert!(live.safety.background_sampling);
        assert!(!live.safety.automatic_download_test);
        assert_eq!(live.overhead.network_cache_ttl_secs, 60);
        assert!(!live.overhead.background_network_probes);
        assert!(!live.overhead.background_download_tests);
        assert!(live.overhead.sampler_runs_total > 0);
        assert_eq!(live.acceptance.modes.len(), 6);
        assert!(live.acceptance.ledger_persisted);
        assert!(live
            .acceptance
            .modes
            .iter()
            .any(|mode| mode.mode == "RESTART_RESTORE"));
        assert_eq!(
            live.overhead.process.memory_evidence,
            EvidenceLevel::Measured
        );
        assert_eq!(live.network.evidence, EvidenceLevel::Measured);
        assert!(live
            .network
            .default_routes
            .iter()
            .any(|route| route.selected));
        assert!(!live.ai_work.processes.is_empty());
        assert!(!live.ai_work.codex_tasks.is_empty());
        assert!(
            live.collection_ms < 15_000,
            "collection took {} ms",
            live.collection_ms
        );
        let selected_routes = live
            .network
            .default_routes
            .iter()
            .filter(|route| route.selected)
            .map(|route| {
                format!(
                    "{}:{}:metric={}:latency={:?}:{}",
                    route.family,
                    route.interface_alias,
                    route.effective_metric,
                    route.latency_ms,
                    route.latency_status
                )
            })
            .collect::<Vec<_>>();
        let process_counts = live
            .ai_work
            .processes
            .iter()
            .map(|process| format!("{}={}", process.app, process.process_count))
            .collect::<Vec<_>>();
        println!(
            "collection_ms={} split_route={} routes=[{}] codex_tasks={} current_tokens={} processes=[{}]",
            live.collection_ms,
            live.network.split_default_route,
            selected_routes.join(", "),
            live.ai_work.codex_tasks.len(),
            live.ai_work
                .codex_tasks
                .first()
                .map(|task| task.tokens_used)
                .unwrap_or(0),
            process_counts.join(", ")
        );
    }
}
