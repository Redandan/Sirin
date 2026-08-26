//! Read-only, on-demand Windows AI work monitor.
//!
//! The collector deliberately does no background work. The web UI calls the
//! dedicated endpoint only while the AI monitor view is open; this module then
//! serves a 15-second cached snapshot. No credentials, command lines, message
//! bodies, or file contents are returned.

use crate::platform::NoWindow;
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SNAPSHOT_TTL: Duration = Duration::from_secs(15);
const CODEX_TASK_LIMIT: usize = 8;
const TOKEN_HISTORY_LIMIT: usize = 12;
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
    pub safety: SafetyView,
}

#[derive(Debug, Clone, Serialize)]
pub struct SafetyView {
    pub local_only: bool,
    pub read_only: bool,
    pub credentials_accessed: bool,
    pub background_sampling: bool,
    pub automatic_download_test: bool,
}

impl Default for SafetyView {
    fn default() -> Self {
        Self {
            local_only: true,
            read_only: true,
            credentials_accessed: false,
            background_sampling: false,
            automatic_download_test: false,
        }
    }
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
    pub sirin_tokens: SirinTokenView,
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
    pub evidence: EvidenceLevel,
    pub history: Vec<TokenTrendPointView>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenTrendPointView {
    pub interval_secs: u64,
    pub delta_tokens: u64,
    pub tokens_per_min: u64,
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

struct SnapshotCache {
    refreshed_at: Instant,
    value: Option<AiMonitorSnapshot>,
}

static SNAPSHOT_CACHE: OnceLock<Mutex<SnapshotCache>> = OnceLock::new();

#[derive(Default)]
struct TokenTrendState {
    sampled_at: Option<Instant>,
    tokens: HashMap<String, u64>,
    history: VecDeque<TokenTrendPointView>,
}

static TOKEN_TREND: OnceLock<Mutex<TokenTrendState>> = OnceLock::new();

/// Return a cached, read-only monitor snapshot. Collection only happens when
/// this function is called by the on-demand web endpoint.
pub fn snapshot() -> AiMonitorSnapshot {
    let cache = SNAPSHOT_CACHE.get_or_init(|| {
        Mutex::new(SnapshotCache {
            refreshed_at: Instant::now() - SNAPSHOT_TTL,
            value: None,
        })
    });
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    if guard.refreshed_at.elapsed() < SNAPSHOT_TTL {
        if let Some(value) = &guard.value {
            return value.clone();
        }
    }

    let started = Instant::now();
    let network = collect_network();
    let ai_work = collect_ai_work();
    let result = AiMonitorSnapshot {
        version: env!("CARGO_PKG_VERSION").to_string(),
        captured_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        collection_ms: started.elapsed().as_millis() as u64,
        cache_ttl_secs: SNAPSHOT_TTL.as_secs(),
        network,
        ai_work,
        safety: SafetyView::default(),
    };
    guard.refreshed_at = Instant::now();
    guard.value = Some(result.clone());
    result
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
    let (tasks, codex_error) = collect_codex_tasks();
    let recent_total = tasks.iter().map(|task| task.tokens_used).sum();
    let trend = update_token_trend(&tasks);
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
        codex_source_error: codex_error.or(process_error),
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

fn update_token_trend(tasks: &[CodexTaskView]) -> TokenTrendView {
    let now = Instant::now();
    let current: HashMap<String, u64> = tasks
        .iter()
        .map(|task| (task.id.clone(), task.tokens_used))
        .collect();
    let cell = TOKEN_TREND.get_or_init(|| Mutex::new(TokenTrendState::default()));
    let mut state = cell.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sampled_at) = state.sampled_at else {
        state.sampled_at = Some(now);
        state.tokens = current;
        return TokenTrendView {
            history: state.history.iter().cloned().collect(),
            ..Default::default()
        };
    };
    let elapsed = now.saturating_duration_since(sampled_at);
    let delta = token_delta(&current, &state.tokens);
    let tokens_per_min = if elapsed.as_secs_f64() > 0.0 {
        (delta as f64 * 60.0 / elapsed.as_secs_f64()).round() as u64
    } else {
        0
    };
    push_token_history(
        &mut state.history,
        TokenTrendPointView {
            interval_secs: elapsed.as_secs(),
            delta_tokens: delta,
            tokens_per_min,
        },
    );
    state.sampled_at = Some(now);
    state.tokens = current;
    TokenTrendView {
        interval_secs: elapsed.as_secs(),
        delta_tokens: delta,
        tokens_per_min,
        // This is derived from two overlapping top-N task snapshots rather
        // than a billing meter or an event stream.
        evidence: EvidenceLevel::Inferred,
        history: state.history.iter().cloned().collect(),
    }
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
        let ipv4_interfaces_text = run_utf8_command("netsh.exe interface ipv4 show interfaces");
        let ipv6_interfaces_text = run_utf8_command("netsh.exe interface ipv6 show interfaces");
        let ipv4_text = run_utf8_command("netsh.exe interface ipv4 show route store=active");
        let ipv6_text = run_utf8_command("netsh.exe interface ipv6 show route store=active");
        let wifi_text = run_utf8_command("netsh.exe wlan show interfaces");

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

        for family in ["IPv4", "IPv6"] {
            if let Some(route) = routes.iter_mut().find(|route| route.family == family) {
                route.selected = true;
                let target = if family == "IPv4" {
                    "1.1.1.1"
                } else {
                    "2606:4700:4700::1111"
                };
                route.latency_target = Some(target.to_string());
                match ping_latency(family, target) {
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
    fn token_history_keeps_latest_twelve_points() {
        let mut history = VecDeque::new();
        for value in 0..15 {
            push_token_history(
                &mut history,
                TokenTrendPointView {
                    interval_secs: 60,
                    delta_tokens: value,
                    tokens_per_min: value,
                },
            );
        }
        assert_eq!(history.len(), TOKEN_HISTORY_LIMIT);
        assert_eq!(history.front().map(|point| point.tokens_per_min), Some(3));
        assert_eq!(history.back().map(|point| point.tokens_per_min), Some(14));
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
        assert!(!live.safety.background_sampling);
        assert!(!live.safety.automatic_download_test);
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

