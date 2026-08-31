//! Physical iPhone control facade owned by Sirin.
//!
//! The low-level Windows driver is deliberately hidden behind this module so
//! MCP clients only depend on Sirin's stable contract.

use chrono::Utc;
use image::{GenericImageView, GrayImage};
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::{Duration, Instant};

use crate::platform::NoWindow;

const DEFAULT_DRIVER_URL: &str = "http://127.0.0.1:8770";
const DEFAULT_LEASE_SECS: u64 = 300;
const MAX_LEASE_SECS: u64 = 900;
const MAX_REMEMBERED_ACTIONS: usize = 100;
const MIN_SCREEN_EDGE: u32 = 100;
const CONTENT_LEFT_PERCENT: u32 = 5;
const CONTENT_RIGHT_PERCENT: u32 = 95;
const CONTENT_TOP_PERCENT: u32 = 8;
const CONTENT_BOTTOM_PERCENT: u32 = 85;
const CONTENT_PIXEL_DELTA_THRESHOLD: u8 = 12;
const MATERIAL_CHANGED_PIXEL_RATIO: f64 = 0.01;
const MIN_VERTICAL_SHIFT_STEPS: i32 = 3;
const MIN_VERTICAL_SHIFT_IMPROVEMENT: f64 = 0.10;
const DRIVER_TASK_NAME: &str = "Sirin iOS Driver";
const DRIVER_MANAGER_POLL_SECS: u64 = 60;

fn is_allowed_acceptance_route(route: &str) -> bool {
    matches!(route, "safari_store" | "telegram_bot" | "codex_remote")
}

static MANAGER: OnceLock<Result<IosDeviceManager, String>> = OnceLock::new();
static DRIVER_MANAGER_STARTED: AtomicBool = AtomicBool::new(false);
static DRIVER_LIFECYCLE: OnceLock<Mutex<DriverLifecycleState>> = OnceLock::new();

#[derive(Debug, Default)]
struct DriverLifecycleState {
    last_checked_at: Option<String>,
    last_start_attempt_at: Option<String>,
    last_result: Option<String>,
    start_attempts: u64,
    consecutive_unreachable: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlPolicy {
    AcceptanceBrowseOnly,
    SupervisedControl,
}

impl ControlPolicy {
    fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.unwrap_or("acceptance_browse_only") {
            "acceptance_browse_only" => Ok(Self::AcceptanceBrowseOnly),
            "supervised_control" => Ok(Self::SupervisedControl),
            other => Err(format!(
                "unsupported policy '{other}'; expected acceptance_browse_only or supervised_control"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::AcceptanceBrowseOnly => "acceptance_browse_only",
            Self::SupervisedControl => "supervised_control",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct CaptureEvidence {
    frame_id: String,
    captured_at: String,
    path: String,
    size_bytes: u64,
    width: u32,
    height: u32,
    sha256: String,
}

#[derive(Debug, Clone, Serialize)]
struct ControlEvidence {
    action: String,
    action_id: String,
    observed_at: String,
    before_frame_id: String,
    after_frame_id: Option<String>,
    changed: Option<bool>,
    visual_change: Option<VisualChangeEvidence>,
    status: String,
    note: String,
}

#[derive(Debug, Clone, Serialize)]
struct ContentRegionEvidence {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Serialize)]
struct VisualChangeEvidence {
    full_frame_sha256_changed: bool,
    content_region: ContentRegionEvidence,
    sample_stride_pixels: u32,
    sampled_pixels: u64,
    changed_pixel_ratio: f64,
    mean_absolute_delta_0_to_255: f64,
    material_content_change: bool,
    best_vertical_shift_pixels: Option<i32>,
    vertical_shift_improvement_ratio: Option<f64>,
    vertical_scroll_proven: Option<bool>,
}

#[derive(Debug, Clone)]
struct Lease {
    session_id: String,
    owner: String,
    policy: ControlPolicy,
    started_at: String,
    expires_at: Instant,
    ttl_secs: u64,
    completed_actions: BTreeMap<String, Value>,
    action_order: VecDeque<String>,
}

#[derive(Debug, Default)]
struct ManagerState {
    lease: Option<Lease>,
    last_capture: Option<CaptureEvidence>,
    last_control: Option<ControlEvidence>,
    recovery_actions: BTreeMap<String, Value>,
    recovery_action_order: VecDeque<String>,
    acceptance_runs: BTreeMap<String, Value>,
    acceptance_run_order: VecDeque<String>,
    last_link_cleanup: Option<Value>,
    link_cleanup_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderHealth {
    Healthy,
    Stale(&'static str),
    Unreachable,
}

#[derive(Debug, Default)]
struct ProviderSnapshot {
    reachable: bool,
    provider_name: String,
    device_detected_reported: Option<bool>,
    device_probe_status: Option<String>,
    acceptance_only: Option<bool>,
    input_reported: Option<bool>,
    link: Option<String>,
    starting: Option<bool>,
    last_link_result: Option<String>,
    lan_exposed: Option<bool>,
    lan_protection: Option<String>,
    attach_mode: Option<String>,
    phone_readable: bool,
    locked: Option<bool>,
    app: Option<String>,
    page: Option<String>,
    battery: Option<Value>,
    status_error: Option<String>,
    phone_error: Option<String>,
}

impl ProviderSnapshot {
    fn device_detected(&self) -> bool {
        self.device_detected_reported == Some(true) || self.link.as_deref() == Some("up")
    }

    fn to_json(&self) -> Value {
        json!({
            "provider": self.provider_name,
            "provider_reachable": self.reachable,
            "acceptance_only": self.acceptance_only,
            "device_probe_status": self.device_probe_status,
            "input_reported": self.input_reported,
            "wda_link_reported": self.link,
            "starting": self.starting,
            "last_link_result": self.last_link_result,
            "lan_exposed": self.lan_exposed,
            "lan_protection": self.lan_protection,
            "attach_mode": self.attach_mode,
            "phone": {
                "locked": self.locked,
                "app": self.app,
                "page": self.page,
                "battery": self.battery,
            },
            "provider_errors": {
                "status": self.status_error,
                "phone": self.phone_error,
            }
        })
    }
}

struct IosDeviceManager {
    base_url: String,
    http: Client,
    state: Mutex<ManagerState>,
    action_gate: tokio::sync::Mutex<()>,
    sequence: AtomicU64,
}

impl IosDeviceManager {
    fn new(base_url: &str) -> Result<Self, String> {
        let base_url = validate_driver_url(base_url)?;
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("failed to create iOS driver HTTP client: {e}"))?;
        Ok(Self {
            base_url,
            http,
            state: Mutex::new(ManagerState::default()),
            action_gate: tokio::sync::Mutex::new(()),
            sequence: AtomicU64::new(1),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn get_json(&self, path: &str) -> Result<Value, String> {
        let response = self
            .http
            .get(self.endpoint(path))
            .send()
            .await
            .map_err(|e| format!("iOS driver GET {path} failed: {e}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("iOS driver GET {path} body failed: {e}"))?;
        if !status.is_success() {
            return Err(provider_http_error("GET", path, status, &body));
        }
        serde_json::from_slice(&body)
            .map_err(|e| format!("iOS driver GET {path} returned invalid JSON: {e}"))
    }

    async fn post_json(&self, path: &str, payload: Value) -> Result<Value, String> {
        let response = self
            .http
            .post(self.endpoint(path))
            .json(&payload)
            .timeout(provider_post_timeout(path))
            .send()
            .await
            .map_err(|e| format!("iOS driver POST {path} failed: {e}"))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("iOS driver POST {path} body failed: {e}"))?;
        if !status.is_success() {
            return Err(provider_http_error("POST", path, status, &body));
        }
        serde_json::from_slice(&body)
            .map_err(|e| format!("iOS driver POST {path} returned invalid JSON: {e}"))
    }

    async fn provider_snapshot(&self) -> ProviderSnapshot {
        if let Ok(combined) = self.get_json("/api/snapshot").await {
            let status_result = combined
                .get("status")
                .cloned()
                .ok_or_else(|| "iOS driver /api/snapshot omitted status".to_string());
            let phone_status = combined["phone_status"].as_u64().unwrap_or(503);
            let phone_result = if phone_status == 200 {
                combined
                    .get("phone")
                    .cloned()
                    .ok_or_else(|| "iOS driver /api/snapshot omitted phone".to_string())
            } else {
                Err(format!(
                    "iOS driver snapshot phone unavailable: {}",
                    combined["phone"]["error"]
                        .as_str()
                        .unwrap_or("MISSING_PROOF")
                ))
            };
            return Self::provider_snapshot_from_results(status_result, phone_result);
        }
        let (status_result, phone_result) =
            tokio::join!(self.get_json("/api/status"), self.get_json("/api/phone"));
        Self::provider_snapshot_from_results(status_result, phone_result)
    }

    fn provider_snapshot_from_results(
        status_result: Result<Value, String>,
        phone_result: Result<Value, String>,
    ) -> ProviderSnapshot {
        let status = status_result.as_ref().ok();
        let phone = phone_result.as_ref().ok();
        let phone_readable = phone.is_some_and(phone_response_is_readable);
        ProviderSnapshot {
            reachable: status.is_some(),
            provider_name: status
                .and_then(|v| v["provider"].as_str())
                .unwrap_or("legacy-private-provider")
                .to_owned(),
            device_detected_reported: status.and_then(|v| v["device_detected"].as_bool()),
            device_probe_status: status
                .and_then(|v| v["device_probe_status"].as_str())
                .map(ToOwned::to_owned),
            acceptance_only: status.and_then(|v| v["acceptance_only"].as_bool()),
            input_reported: status.and_then(|v| v["input"].as_bool()),
            link: status
                .and_then(|v| v["link"].as_str())
                .map(ToOwned::to_owned),
            starting: status.and_then(|v| v["starting"].as_bool()),
            last_link_result: status
                .and_then(|v| v["last_link_result"].as_str())
                .map(ToOwned::to_owned),
            lan_exposed: status.and_then(|v| v["lan_exposed"].as_bool()),
            lan_protection: status
                .and_then(|v| v["lan_protection"].as_str())
                .map(ToOwned::to_owned),
            attach_mode: status
                .and_then(|v| v["attach_mode"].as_str())
                .map(ToOwned::to_owned),
            phone_readable,
            locked: phone.and_then(|v| v["locked"].as_bool()),
            app: phone
                .and_then(|v| v["app"].as_str().or_else(|| v["app"]["bundleId"].as_str()))
                .map(ToOwned::to_owned),
            page: phone
                .and_then(|v| v["page"].as_str())
                .map(ToOwned::to_owned),
            battery: phone.and_then(|v| v.get("battery")).cloned(),
            status_error: status_result.err(),
            phone_error: phone_result.err(),
        }
    }

    async fn provider_health(&self) -> ProviderHealth {
        if let Ok(health) = self.get_json("/api/healthz").await {
            return classify_provider_healthz(&health);
        }
        match self.get_json("/api/status").await {
            Ok(status) if status["provider"].as_str() == Some("sirin-ios-driver") => {
                ProviderHealth::Stale("STALE_PROVIDER_LEGACY_API")
            }
            Ok(_) => ProviderHealth::Stale("STALE_PROVIDER_IDENTITY_MISMATCH"),
            Err(_) => ProviderHealth::Unreachable,
        }
    }

    async fn status(&self) -> Value {
        let provider = self.provider_snapshot().await;
        let provider_contract_ok = passive_provider_contract_ok(&provider);
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let lease = state.lease.as_ref().map(lease_json);
        json!({
            "ok": provider_contract_ok,
            "provider_contract_status": if provider_contract_ok { "PASS" } else { "STALE_OR_UNPROVEN" },
            "provider": provider.to_json(),
            "capabilities": {
                "DEVICE_DETECTED": proof(provider.device_detected(), "fresh provider device information or a live WDA link"),
                "INFO_READABLE": proof(provider.phone_readable, "fresh /api/phone response"),
                "SCREEN_CAPTURE": state.last_capture.as_ref().map(|e| json!({
                    "status": "PASS", "evidence": e
                })).unwrap_or_else(|| json!({"status": "MISSING_PROOF"})),
                "SCREEN_CONTROL": state.last_control.as_ref().map(|e| json!({
                    "status": e.status, "evidence": e
                })).unwrap_or_else(|| json!({"status": "MISSING_PROOF"})),
            },
            "active_lease": lease,
            "last_link_cleanup": state.last_link_cleanup,
            "link_cleanup_required": state.link_cleanup_required,
            "lifecycle": driver_lifecycle_json(),
            "human_blockers": human_blockers(&provider),
            "safety": {
                "local_only_required": true,
                "credentials_supported": false,
                "unlock_supported": false,
                "jailbreak_supported": false,
            }
        })
    }

    async fn capture(&self, label: &str) -> Result<CaptureEvidence, String> {
        let response = self
            .http
            .get(self.endpoint("/api/screenshot"))
            .send()
            .await
            .map_err(|e| format!("iOS screenshot request failed: {e}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("iOS screenshot body failed: {e}"))?;
        if !status.is_success() {
            return Err(provider_http_error(
                "GET",
                "/api/screenshot",
                status,
                &bytes,
            ));
        }
        let image = image::load_from_memory(&bytes)
            .map_err(|e| format!("iOS screenshot is not a valid image: {e}"))?;
        let (width, height) = image.dimensions();
        if width < MIN_SCREEN_EDGE || height < MIN_SCREEN_EDGE {
            return Err(format!(
                "MISSING_PROOF: iOS screenshot is only {width}x{height}; refusing placeholder evidence"
            ));
        }

        let dir = crate::platform::app_data_dir()
            .join("device_evidence")
            .join("ios");
        fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create iOS evidence dir {}: {e}", dir.display()))?;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let frame_id = format!(
            "IOSFRAME-{}-{sequence}",
            Utc::now().format("%Y%m%d%H%M%S%3f")
        );
        let path = dir.join(format!("{}_{}.png", frame_id, sanitize_file_part(label)));
        fs::write(&path, &bytes)
            .map_err(|e| format!("failed to write iOS screenshot {}: {e}", path.display()))?;
        let evidence = CaptureEvidence {
            frame_id,
            captured_at: Utc::now().to_rfc3339(),
            path: path.display().to_string(),
            size_bytes: bytes.len() as u64,
            width,
            height,
            sha256: sha256_hex(&bytes),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.last_capture = Some(evidence.clone());
        Ok(evidence)
    }

    async fn release_link_locked(&self, reason: &str, session_id: Option<&str>) -> Value {
        let provider_release = match self.post_json("/api/release-link", json!({})).await {
            Ok(value) => value,
            Err(error) => json!({"ok": false, "error": error}),
        };
        let ok = provider_release["ok"].as_bool() == Some(true);
        let record = json!({
            "ok": ok,
            "reason": reason,
            "session_id": session_id,
            "recorded_at": Utc::now().to_rfc3339(),
            "provider_release": provider_release,
        });
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.link_cleanup_required = !ok;
        state.last_link_cleanup = Some(record);
        provider_release
    }

    async fn release_expired_lease_locked(&self, expected_id: Option<&str>) -> Option<Value> {
        let expired_session_id = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            take_expired_lease(&mut state, expected_id).map(|lease| lease.session_id)
        };
        match expired_session_id {
            Some(session_id) => Some(
                self.release_link_locked("lease_ttl_expired", Some(&session_id))
                    .await,
            ),
            None => None,
        }
    }

    async fn retry_pending_link_cleanup_locked(&self) -> Result<(), String> {
        let pending = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .link_cleanup_required;
        if !pending {
            return Ok(());
        }
        let release = self
            .release_link_locked("retry_pending_cleanup", None)
            .await;
        if release["ok"].as_bool() == Some(true) {
            Ok(())
        } else {
            Err("LINK_CLEANUP_REQUIRED: the previous iPhone link release is still unproven".into())
        }
    }

    async fn agoramarket_webview_evidence(&self) -> Result<Value, String> {
        let provider = self.provider_snapshot().await;
        if !provider.reachable {
            return Err("MISSING_PROOF: Sirin iPhone provider is unavailable".into());
        }
        if provider.locked != Some(false) {
            return Err("MISSING_PROOF: the iPhone must be unlocked and readable".into());
        }
        if provider.app.as_deref() != Some("ph.telegra.Telegraph") {
            return Err("MISSING_PROOF: Telegram is not the foreground application".into());
        }
        if provider.link.as_deref() != Some("up") || provider.input_reported != Some(true) {
            return Err(link_unready_error(&provider));
        }

        let frame = self.capture("agoramarket-webview-evidence").await?;
        let live = self.get_json("/api/agoramarket-webview-evidence").await?;
        Ok(json!({
            "ok": true,
            "evidence_kind": "fresh physical Telegram WebView runtime",
            "frame": frame,
            "live_webview": live,
            "note": "The fixed probe reads only the AgoraMarket origin, one bootstrap version key, and non-secret runtime/layout facts; it does not expose arbitrary JavaScript or prove old-cache behavior without a known-old precondition."
        }))
    }

    async fn start_session(&self, args: Value) -> Result<Value, String> {
        let policy = ControlPolicy::parse(args["policy"].as_str())?;
        if policy == ControlPolicy::SupervisedControl
            && std::env::var("SIRIN_IOS_SUPERVISED_CONTROL").as_deref() != Ok("1")
        {
            return Err(
                "POLICY_DENIED: supervised_control requires SIRIN_IOS_SUPERVISED_CONTROL=1".into(),
            );
        }
        let ttl_secs = args["ttl_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_LEASE_SECS)
            .clamp(30, MAX_LEASE_SECS);
        let owner = sanitize_owner(args["owner"].as_str().unwrap_or("codex"));
        let _action_guard = self.action_gate.lock().await;
        if let Some(cleanup) = self.release_expired_lease_locked(None).await {
            if cleanup["ok"].as_bool() != Some(true) {
                return Err(
                    "LINK_CLEANUP_REQUIRED: an expired lease could not release its iPhone link"
                        .into(),
                );
            }
        }
        self.retry_pending_link_cleanup_locked().await?;

        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = state.lease.as_ref() {
                return Err(format!(
                    "DEVICE_BUSY: active lease belongs to '{}' with {} seconds remaining",
                    active.owner,
                    active
                        .expires_at
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                ));
            }
        }

        let mut link_release_required_on_failure = false;
        let result = async {
            let mut provider = self.provider_snapshot().await;
            validate_provider_contract(&provider, policy)?;
            if provider.lan_exposed != Some(false) {
                return Err(lan_safety_error(&provider));
            }
            if !provider.device_detected() {
                return Err(link_unready_error(&provider));
            }
            link_release_required_on_failure = provider.link.as_deref() == Some("up")
                || provider.input_reported == Some(true);
            if provider.link.as_deref() != Some("up") || provider.input_reported != Some(true) {
                if let Some(error) = link_activation_safety_error(&provider) {
                    return Err(error);
                }
                link_release_required_on_failure = true;
                let _ = self.post_json("/api/ensure-link", json!({})).await?;
                provider = self.provider_snapshot().await;
                validate_provider_contract(&provider, policy)?;
                if provider.lan_exposed != Some(false) {
                    return Err(lan_safety_error(&provider));
                }
                if provider.link.as_deref() != Some("up")
                    || provider.input_reported != Some(true)
                {
                    return Err(link_unready_error(&provider));
                }
            }
            match (provider.phone_readable, provider.locked) {
                (true, Some(false)) => {}
                (_, Some(true)) => {
                    return Err(
                        "NEEDS_HUMAN_UNLOCK: unlock the iPhone personally, then retry".into(),
                    )
                }
                _ => {
                    return Err(
                        "MISSING_PROOF: iPhone lock and information state is not freshly readable"
                            .into(),
                    )
                }
            }

            let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
            let session_id = format!(
                "IOSSESSION-{}-{sequence}",
                Utc::now().format("%Y%m%d%H%M%S%3f")
            );
            let lease = Lease {
                session_id: session_id.clone(),
                owner,
                policy,
                started_at: Utc::now().to_rfc3339(),
                expires_at: Instant::now() + Duration::from_secs(ttl_secs),
                ttl_secs,
                completed_actions: BTreeMap::new(),
                action_order: VecDeque::new(),
            };
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = state.lease.as_ref() {
                return Err(format!(
                    "DEVICE_BUSY: active lease belongs to '{}' with {} seconds remaining",
                    active.owner,
                    active
                        .expires_at
                        .saturating_duration_since(Instant::now())
                        .as_secs()
                ));
            }
            state.lease = Some(lease);
            Ok(json!({
                "ok": true,
                "session_id": session_id,
                "policy": policy.as_str(),
                "ttl_secs": ttl_secs,
                "provider": provider.provider_name,
                "note": "This lease never authorizes passcodes, trust prompts, signing repair, messaging, orders, or payment."
            }))
        }
        .await;

        let result = match result {
            Ok(value) => value,
            Err(error) => {
                if link_release_required_on_failure {
                    let cleanup = self.release_link_locked("session_start_failed", None).await;
                    if cleanup["ok"].as_bool() != Some(true) {
                        return Err(format!(
                            "{error}; LINK_CLEANUP_REQUIRED: activation failure cleanup is unproven"
                        ));
                    }
                }
                return Err(error);
            }
        };
        let session_id = result["session_id"]
            .as_str()
            .expect("successful session has session_id")
            .to_string();
        schedule_lease_expiry(session_id, ttl_secs);
        Ok(result)
    }

    fn session_status(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        json!({
            "active_lease": state.lease.as_ref().map(lease_json),
            "last_control": state.last_control,
            "last_link_cleanup": state.last_link_cleanup,
            "link_cleanup_required": state.link_cleanup_required,
        })
    }

    async fn stop_session(&self, args: Value) -> Result<Value, String> {
        let session_id = required_str(&args, "session_id")?;
        if args["release_link"].as_bool() == Some(false) {
            return Err(
                "POLICY_DENIED: public session stop always releases the transient iPhone link"
                    .into(),
            );
        }
        let _action_guard = self.action_gate.lock().await;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match state.lease.as_ref() {
                Some(lease) if lease.session_id == session_id => {
                    state.lease = None;
                }
                Some(_) => {
                    return Err(
                        "LEASE_MISMATCH: session_id does not own the active device lease".into(),
                    );
                }
                None => return Err("NO_ACTIVE_LEASE".into()),
            }
        }
        let link_release = self
            .release_link_locked("session_stop", Some(session_id))
            .await;
        let ok = link_release["ok"].as_bool() == Some(true);
        Ok(json!({
            "ok": ok,
            "session_id": session_id,
            "status": if ok { "released" } else { "lease_released_link_release_failed" },
            "link_release": link_release,
        }))
    }

    async fn recover_home(&self, args: Value) -> Result<Value, String> {
        let action_id = validate_action_id(required_str(&args, "action_id")?)?;
        let _action_guard = self.action_gate.lock().await;

        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(active) = state.lease.as_ref() {
                return Err(format!(
                    "DEVICE_BUSY: recovery is unavailable while lease '{}' is active",
                    active.session_id
                ));
            }
            if let Some(previous) = state.recovery_actions.get(&action_id) {
                let mut replay = previous.clone();
                if let Some(obj) = replay.as_object_mut() {
                    obj.insert("idempotent_replay".into(), Value::Bool(true));
                }
                return Ok(replay);
            }
        }

        let provider = self.provider_snapshot().await;
        if !provider.reachable {
            return Err(format!(
                "MISSING_PROOF: Sirin iOS Driver is unreachable: {}",
                provider
                    .status_error
                    .unwrap_or_else(|| "unknown error".into())
            ));
        }
        if provider.provider_name != "sirin-ios-driver" {
            return Err("POLICY_DENIED: recovery requires the Sirin-owned iOS Driver".into());
        }
        if provider.lan_exposed != Some(false) {
            return Err("POLICY_DENIED: recovery requires a confirmed loopback-only driver".into());
        }
        if provider.acceptance_only != Some(true) {
            return Err("POLICY_DENIED: recovery requires acceptance-only driver mode".into());
        }
        if !provider.device_detected() {
            return Err("NEEDS_HUMAN_USB: no physical iPhone is currently detected".into());
        }
        if provider.starting == Some(true) {
            return Err(
                "DEVICE_BUSY: the provider is already starting WDA; re-observe before recovery"
                    .into(),
            );
        }
        if provider.link.as_deref() == Some("up") || provider.input_reported == Some(true) {
            return Err(
                "POLICY_DENIED: WDA is healthy; acquire a lease and use ios_home instead".into(),
            );
        }
        if !matches!(provider.link.as_deref(), Some("down" | "wedged")) {
            return Err("MISSING_PROOF: provider did not confirm a down or wedged WDA link".into());
        }

        let before = self.capture("recover-home-before").await?;
        let provider_result = match self.post_json("/api/recover-home", json!({})).await {
            Ok(value) => value,
            Err(error) => {
                let evidence = ControlEvidence {
                    action: "recover_home".into(),
                    action_id: action_id.clone(),
                    observed_at: Utc::now().to_rfc3339(),
                    before_frame_id: before.frame_id.clone(),
                    after_frame_id: None,
                    changed: None,
                    visual_change: None,
                    status: "MISSING_PROOF".into(),
                    note: format!(
                        "Recovery call did not produce a confirmed result; do not retry with a new action_id: {error}"
                    ),
                };
                let result = json!({
                    "ok": false,
                    "action": "recover_home",
                    "provider_error": error,
                    "RECOVERY_HOME": "MISSING_PROOF",
                    "screen_control": evidence,
                    "idempotent_replay": false,
                });
                self.remember_recovery_action(&action_id, result.clone(), evidence);
                return Ok(result);
            }
        };

        let settle_ms = args["settle_ms"]
            .as_u64()
            .unwrap_or(1_500)
            .clamp(200, 10_000);
        tokio::time::sleep(Duration::from_millis(settle_ms)).await;
        let after_result = self.capture("recover-home-after").await;
        let (after, changed, visual_change, status, note) = match after_result {
            Ok(after) => {
                match prove_control_change("recover_home", &before, &after) {
                    Ok((changed, visual_change, note)) => {
                        let status = if changed { "PASS" } else { "MISSING_PROOF" };
                        (Some(after), Some(changed), Some(visual_change), status, note)
                    }
                    Err(error) => (
                        Some(after),
                        None,
                        None,
                        "MISSING_PROOF",
                        format!(
                            "The provider accepted recovery, but content-region comparison failed: {error}"
                        ),
                    ),
                }
            }
            Err(error) => (
                None,
                None,
                None,
                "MISSING_PROOF",
                format!("The provider accepted recovery, but post-action capture failed: {error}"),
            ),
        };
        let evidence = ControlEvidence {
            action: "recover_home".into(),
            action_id: action_id.clone(),
            observed_at: Utc::now().to_rfc3339(),
            before_frame_id: before.frame_id.clone(),
            after_frame_id: after.as_ref().map(|value| value.frame_id.clone()),
            changed,
            visual_change,
            status: status.into(),
            note,
        };
        let result = json!({
            "ok": true,
            "action": "recover_home",
            "provider_result": provider_result,
            "before": before,
            "after": after,
            "RECOVERY_HOME": status,
            "screen_control": evidence,
            "idempotent_replay": false,
        });
        self.remember_recovery_action(&action_id, result.clone(), evidence);
        Ok(result)
    }

    async fn perform_action(
        &self,
        args: &Value,
        action: &str,
        path: &str,
        payload: Value,
        acceptance_allowed: bool,
        settle_ms: u64,
    ) -> Result<Value, String> {
        let session_id = required_str(args, "session_id")?.to_string();
        let action_id = validate_action_id(required_str(args, "action_id")?)?;
        let _action_guard = self.action_gate.lock().await;

        let policy = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let lease = state.lease.as_ref().ok_or("NO_ACTIVE_LEASE")?;
            if lease.expires_at <= Instant::now() {
                return Err("LEASE_EXPIRED: automatic link cleanup is pending".into());
            }
            if lease.session_id != session_id {
                return Err(
                    "LEASE_MISMATCH: session_id does not own the active device lease".into(),
                );
            }
            if let Some(previous) = lease.completed_actions.get(&action_id) {
                let mut replay = previous.clone();
                if let Some(obj) = replay.as_object_mut() {
                    obj.insert("idempotent_replay".into(), Value::Bool(true));
                }
                return Ok(replay);
            }
            lease.policy
        };

        if policy == ControlPolicy::AcceptanceBrowseOnly && !acceptance_allowed {
            return Err(format!(
                "POLICY_DENIED: action '{action}' is unavailable in acceptance_browse_only"
            ));
        }

        let before = self.capture(&format!("{action}-before")).await?;
        if let Some(expected) = args["expected_frame_sha256"].as_str() {
            if expected != before.sha256 {
                return Err(format!(
                    "STALE_FRAME: expected {expected}, current frame is {}",
                    before.sha256
                ));
            }
        }

        let provider_result = match self.post_json(path, payload).await {
            Ok(value) => value,
            Err(error) => {
                let evidence = ControlEvidence {
                    action: action.into(),
                    action_id: action_id.clone(),
                    observed_at: Utc::now().to_rfc3339(),
                    before_frame_id: before.frame_id.clone(),
                    after_frame_id: None,
                    changed: None,
                    visual_change: None,
                    status: "MISSING_PROOF".into(),
                    note: format!(
                        "Provider call did not produce a confirmed result; do not retry with a new action_id: {error}"
                    ),
                };
                let result = json!({
                    "ok": false,
                    "action": action,
                    "provider_error": error,
                    "screen_control": evidence,
                });
                self.remember_action(&session_id, &action_id, result.clone(), evidence)?;
                return Ok(result);
            }
        };
        let provider_ok = provider_action_confirmed(&provider_result);

        tokio::time::sleep(Duration::from_millis(settle_ms.clamp(200, 10_000))).await;
        let after_result = self.capture(&format!("{action}-after")).await;
        let (after, changed, visual_change, mut status, mut note) = match after_result {
            Ok(after) => {
                match prove_control_change(action, &before, &after) {
                    Ok((changed, visual_change, note)) => {
                        let status = if changed { "PASS" } else { "MISSING_PROOF" };
                        (Some(after), Some(changed), Some(visual_change), status, note)
                    }
                    Err(error) => (
                        Some(after),
                        None,
                        None,
                        "MISSING_PROOF",
                        format!(
                            "The provider accepted the action, but content-region comparison failed: {error}"
                        ),
                    ),
                }
            }
            Err(error) => (
                None,
                None,
                None,
                "MISSING_PROOF",
                format!(
                    "The provider accepted the action, but post-action capture failed: {error}"
                ),
            ),
        };
        if !provider_ok {
            status = "MISSING_PROOF";
            let provider_state = provider_result["state"]
                .as_str()
                .unwrap_or("provider_ok_false");
            note = format!(
                "The private provider did not confirm the bounded action; handle its structured result as a blocker (state={provider_state})."
            );
        }
        let evidence = ControlEvidence {
            action: action.into(),
            action_id: action_id.clone(),
            observed_at: Utc::now().to_rfc3339(),
            before_frame_id: before.frame_id.clone(),
            after_frame_id: after.as_ref().map(|value| value.frame_id.clone()),
            changed,
            visual_change,
            status: status.into(),
            note,
        };
        let result = json!({
            "ok": provider_ok && after.is_some(),
            "action": action,
            "provider_result": provider_result,
            "before": before,
            "after": after,
            "screen_control": evidence,
            "idempotent_replay": false,
        });
        self.remember_action(&session_id, &action_id, result.clone(), evidence)?;
        Ok(result)
    }

    fn remember_action(
        &self,
        session_id: &str,
        action_id: &str,
        result: Value,
        evidence: ControlEvidence,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let lease = state.lease.as_mut().ok_or("NO_ACTIVE_LEASE")?;
        if lease.session_id != session_id {
            return Err("LEASE_MISMATCH after action".into());
        }
        lease.completed_actions.insert(action_id.into(), result);
        lease.action_order.push_back(action_id.into());
        while lease.action_order.len() > MAX_REMEMBERED_ACTIONS {
            if let Some(oldest) = lease.action_order.pop_front() {
                lease.completed_actions.remove(&oldest);
            }
        }
        state.last_control = Some(evidence);
        Ok(())
    }

    fn remember_recovery_action(&self, action_id: &str, result: Value, evidence: ControlEvidence) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.recovery_actions.insert(action_id.into(), result);
        state.recovery_action_order.push_back(action_id.into());
        while state.recovery_action_order.len() > MAX_REMEMBERED_ACTIONS {
            if let Some(oldest) = state.recovery_action_order.pop_front() {
                state.recovery_actions.remove(&oldest);
            }
        }
        state.last_control = Some(evidence);
    }

    fn acceptance_run_result(&self, run_id: &str, route: &str) -> Result<Option<Value>, String> {
        let replay = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acceptance_runs
            .get(run_id)
            .cloned();
        if replay
            .as_ref()
            .is_some_and(|value| value["route"].as_str() != Some(route))
        {
            return Err(
                "IDEMPOTENCY_CONFLICT: run_id was already used for a different acceptance route"
                    .into(),
            );
        }
        Ok(replay)
    }

    fn remember_acceptance_run(&self, run_id: &str, result: Value) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.acceptance_runs.insert(run_id.into(), result);
        state.acceptance_run_order.push_back(run_id.into());
        while state.acceptance_run_order.len() > MAX_REMEMBERED_ACTIONS {
            if let Some(oldest) = state.acceptance_run_order.pop_front() {
                state.acceptance_runs.remove(&oldest);
            }
        }
    }
}

fn manager() -> Result<&'static IosDeviceManager, String> {
    MANAGER
        .get_or_init(|| {
            let url =
                std::env::var("SIRIN_IOS_DRIVER_URL").unwrap_or_else(|_| DEFAULT_DRIVER_URL.into());
            IosDeviceManager::new(&url)
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn validate_provider_contract(
    provider: &ProviderSnapshot,
    policy: ControlPolicy,
) -> Result<(), String> {
    if !provider.reachable {
        return Err(format!(
            "MISSING_PROOF: Sirin iOS Driver is unreachable: {}",
            provider.status_error.as_deref().unwrap_or("unknown error")
        ));
    }
    if provider.provider_name != "sirin-ios-driver" {
        return Err("MISSING_PROOF: loopback iPhone provider identity is not Sirin".into());
    }
    if provider.attach_mode.as_deref() != Some("passive") {
        return Err(
            "POLICY_DENIED: iPhone provider must use the passive attach safety contract".into(),
        );
    }
    match policy {
        ControlPolicy::AcceptanceBrowseOnly if provider.acceptance_only != Some(true) => {
            Err("POLICY_DENIED: acceptance requires a fail-closed acceptance-only provider".into())
        }
        ControlPolicy::SupervisedControl if provider.acceptance_only != Some(false) => Err(
            "POLICY_DENIED: supervised control requires an explicitly supervised provider".into(),
        ),
        _ => Ok(()),
    }
}

fn passive_provider_contract_ok(provider: &ProviderSnapshot) -> bool {
    provider.reachable
        && provider.provider_name == "sirin-ios-driver"
        && provider.acceptance_only == Some(true)
        && provider.attach_mode.as_deref() == Some("passive")
}

fn schedule_lease_expiry(session_id: String, ttl_secs: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(ttl_secs)).await;
        let Ok(manager) = manager() else {
            return;
        };
        let _action_guard = manager.action_gate.lock().await;
        let _ = manager
            .release_expired_lease_locked(Some(&session_id))
            .await;
    });
}

/// Start Sirin's conservative internal provider supervisor.
///
/// The transition provider still runs from its existing scheduled task, but
/// Sirin owns the health decision and start request. The manager never stops,
/// re-signs, installs, unlocks, or exposes the provider. Enable it explicitly
/// with `--ios-driver-autostart` (preferred for the scheduled daemon) or
/// `SIRIN_IOS_DRIVER_AUTOSTART=1` after the daemon rollout is verified.
pub fn spawn_driver_manager_loop() {
    if !driver_autostart_enabled() {
        tracing::info!(target: "sirin", "[ios-driver] Sirin lifecycle manager disabled");
        return;
    }
    if DRIVER_MANAGER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    tokio::spawn(async {
        tracing::info!(target: "sirin", "[ios-driver] Sirin lifecycle manager enabled");
        loop {
            if let Err(error) = driver_manager_tick().await {
                tracing::warn!(target: "sirin", "[ios-driver] lifecycle tick failed: {error}");
            }
            tokio::time::sleep(Duration::from_secs(DRIVER_MANAGER_POLL_SECS)).await;
        }
    });
}

async fn driver_manager_tick() -> Result<(), String> {
    let provider_health = manager()?.provider_health().await;
    {
        let mut state = driver_lifecycle_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.last_checked_at = Some(Utc::now().to_rfc3339());
        match provider_health {
            ProviderHealth::Healthy => {
                state.consecutive_unreachable = 0;
                state.last_result = Some("HEALTHY".into());
                return Ok(());
            }
            ProviderHealth::Stale(result) => {
                state.consecutive_unreachable = 0;
                state.last_result = Some(result.into());
                return Ok(());
            }
            ProviderHealth::Unreachable => {}
        }
        state.consecutive_unreachable = state.consecutive_unreachable.saturating_add(1);
        state.last_start_attempt_at = Some(Utc::now().to_rfc3339());
        state.start_attempts = state.start_attempts.saturating_add(1);
        state.last_result = Some("START_REQUESTED".into());
    }

    start_internal_driver_task().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let recovered = manager()?.provider_health().await == ProviderHealth::Healthy;
    let mut state = driver_lifecycle_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    state.last_result = Some(if recovered {
        state.consecutive_unreachable = 0;
        "RECOVERED".into()
    } else {
        "STARTED_NOT_YET_HEALTHY".into()
    });
    Ok(())
}

fn classify_provider_healthz(value: &Value) -> ProviderHealth {
    if value["ok"].as_bool() != Some(true) || value["provider"].as_str() != Some("sirin-ios-driver")
    {
        return ProviderHealth::Stale("STALE_PROVIDER_IDENTITY_MISMATCH");
    }
    if value["acceptance_only"].as_bool() != Some(true)
        || value["attach_mode"].as_str() != Some("passive")
        || value["bind"].as_str() != Some("127.0.0.1")
    {
        return ProviderHealth::Stale("STALE_PROVIDER_SAFETY_CONTRACT");
    }
    ProviderHealth::Healthy
}

#[cfg(windows)]
async fn start_internal_driver_task() -> Result<(), String> {
    let output = tokio::process::Command::new("schtasks.exe")
        .no_window()
        .args(["/Run", "/TN", DRIVER_TASK_NAME])
        .output()
        .await
        .map_err(|e| format!("failed to request internal iOS driver start: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "internal iOS driver start request failed with {}: {}",
            output.status,
            detail.chars().take(300).collect::<String>()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
async fn start_internal_driver_task() -> Result<(), String> {
    Err("the physical iPhone provider is supported only by Sirin on Windows".into())
}

fn driver_lifecycle_state() -> &'static Mutex<DriverLifecycleState> {
    DRIVER_LIFECYCLE.get_or_init(|| Mutex::new(DriverLifecycleState::default()))
}

fn driver_autostart_enabled() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let env_value = std::env::var("SIRIN_IOS_DRIVER_AUTOSTART").ok();
    driver_autostart_requested(&args, env_value.as_deref())
}

fn driver_autostart_requested(args: &[String], env_value: Option<&str>) -> bool {
    args.iter().any(|arg| arg == "--ios-driver-autostart")
        || env_value
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

fn driver_lifecycle_json() -> Value {
    let state = driver_lifecycle_state()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    json!({
        "owner": "sirin",
        "provider_private": true,
        "autostart_enabled": driver_autostart_enabled(),
        "manager_loop_started": DRIVER_MANAGER_STARTED.load(Ordering::Relaxed),
        "mode": "transitional_scheduled_task",
        "last_checked_at": state.last_checked_at,
        "last_start_attempt_at": state.last_start_attempt_at,
        "last_result": state.last_result,
        "start_attempts": state.start_attempts,
        "consecutive_unreachable": state.consecutive_unreachable,
        "repair_scope": "start-only; no stop, install, signing, trust, unlock, or firewall mutation"
    })
}

pub async fn call_ios_device_status(_args: Value) -> Result<Value, String> {
    Ok(manager()?.status().await)
}

pub async fn call_ios_screen_capture(args: Value) -> Result<Value, String> {
    let label = args["label"].as_str().unwrap_or("observe");
    let evidence = manager()?.capture(label).await?;
    Ok(json!({
        "ok": true,
        "SCREEN_CAPTURE": "PASS",
        "evidence": evidence,
        "note": "A screenshot proves capture only; it does not prove phone control."
    }))
}

pub async fn call_ios_agoramarket_webview_evidence(_args: Value) -> Result<Value, String> {
    manager()?.agoramarket_webview_evidence().await
}

pub async fn call_ios_control_session_start(args: Value) -> Result<Value, String> {
    manager()?.start_session(args).await
}

pub fn call_ios_control_session_status(_args: Value) -> Result<Value, String> {
    Ok(manager()?.session_status())
}

pub async fn call_ios_control_session_stop(args: Value) -> Result<Value, String> {
    manager()?.stop_session(args).await
}

pub async fn call_ios_swipe(args: Value) -> Result<Value, String> {
    let x1 = required_f64(&args, "x1")?;
    let y1 = required_f64(&args, "y1")?;
    let x2 = required_f64(&args, "x2")?;
    let y2 = required_f64(&args, "y2")?;
    let seconds = args["seconds"].as_f64().unwrap_or(0.3).clamp(0.05, 3.0);
    let policy = active_policy(manager()?, &args)?;
    if policy == ControlPolicy::AcceptanceBrowseOnly {
        let dx = (x2 - x1).abs();
        let dy = (y2 - y1).abs();
        if dy < 80.0 || dx > dy / 2.0 {
            return Err(
                "POLICY_DENIED: acceptance_browse_only permits vertical scrolling only".into(),
            );
        }
    }
    manager()?
        .perform_action(
            &args,
            "swipe",
            "/api/swipe",
            json!({"x1": x1, "y1": y1, "x2": x2, "y2": y2, "seconds": seconds}),
            true,
            args["settle_ms"].as_u64().unwrap_or(900),
        )
        .await
}

pub async fn call_ios_home(args: Value) -> Result<Value, String> {
    manager()?
        .perform_action(
            &args,
            "home",
            "/api/home",
            json!({}),
            true,
            args["settle_ms"].as_u64().unwrap_or(1_200),
        )
        .await
}

pub async fn call_ios_recover_home(args: Value) -> Result<Value, String> {
    manager()?.recover_home(args).await
}

pub async fn call_ios_open_app(args: Value) -> Result<Value, String> {
    let app = required_str(&args, "app")?.trim().to_ascii_lowercase();
    let provider_name = match app.as_str() {
        "safari" | "com.apple.mobilesafari" => "Safari",
        "telegram" | "ph.telegra.telegraph" => "Telegram",
        _ => return Err("POLICY_DENIED: P0 permits Safari or Telegram only".into()),
    };
    manager()?
        .perform_action(
            &args,
            "open_app",
            "/api/open-app",
            json!({"name": provider_name}),
            true,
            args["settle_ms"].as_u64().unwrap_or(3_000),
        )
        .await
}

pub async fn call_ios_open_route(args: Value) -> Result<Value, String> {
    let route = required_str(&args, "route")?;
    if !is_allowed_acceptance_route(route) {
        return Err(
            "POLICY_DENIED: route must be safari_store, telegram_bot, or codex_remote".into(),
        );
    }
    manager()?
        .perform_action(
            &args,
            "open_route",
            "/api/acceptance-route",
            json!({"target": route}),
            true,
            args["settle_ms"].as_u64().unwrap_or(5_000),
        )
        .await
}

pub async fn call_ios_acceptance_run(args: Value) -> Result<Value, String> {
    let route = required_str(&args, "route")?.to_string();
    if !is_allowed_acceptance_route(&route) {
        return Err(
            "POLICY_DENIED: route must be safari_store, telegram_bot, or codex_remote".into(),
        );
    }
    let run_id = validate_action_id(required_str(&args, "run_id")?)?;
    let manager = manager()?;
    if let Some(mut replay) = manager.acceptance_run_result(&run_id, &route)? {
        if let Some(object) = replay.as_object_mut() {
            object.insert("idempotent_replay".into(), Value::Bool(true));
        }
        return Ok(replay);
    }

    let started = manager
        .start_session(json!({
            "owner": args["owner"].as_str().unwrap_or("codex-acceptance-run"),
            "policy": "acceptance_browse_only",
            "ttl_secs": args["ttl_secs"].as_u64().unwrap_or(DEFAULT_LEASE_SECS),
        }))
        .await?;
    let session_id = started["session_id"]
        .as_str()
        .ok_or("Sirin created an iPhone lease without a session_id")?
        .to_string();
    let action_result = manager
        .perform_action(
            &json!({
                "session_id": session_id,
                "action_id": run_id,
            }),
            "open_route",
            "/api/acceptance-route",
            json!({"target": route.clone()}),
            true,
            args["settle_ms"].as_u64().unwrap_or(1_000),
        )
        .await;
    let release = manager
        .stop_session(json!({"session_id": session_id}))
        .await;

    let release_ok = release
        .as_ref()
        .ok()
        .and_then(|value| value["ok"].as_bool())
        == Some(true);
    let result = match action_result {
        Ok(action) => json!({
            "ok": action["ok"].as_bool() == Some(true) && release_ok,
            "run_id": run_id,
            "route": route,
            "action": action,
            "lease_release": release.unwrap_or_else(|error| json!({"ok": false, "error": error})),
            "persistent_changes": [],
            "transient_changes": ["foreground application or page", "route UI state"],
            "idempotent_replay": false,
        }),
        Err(error) => json!({
            "ok": false,
            "run_id": run_id,
            "route": route,
            "error": error,
            "lease_release": release.unwrap_or_else(|release_error| json!({"ok": false, "error": release_error})),
            "persistent_changes": [],
            "transient_changes": ["WDA session may have been activated"],
            "idempotent_replay": false,
        }),
    };
    manager.remember_acceptance_run(&run_id, result.clone());
    Ok(result)
}

pub async fn call_ios_tap(args: Value) -> Result<Value, String> {
    let x = required_f64(&args, "x")?;
    let y = required_f64(&args, "y")?;
    if args["expected_frame_sha256"].as_str().is_none() {
        return Err("expected_frame_sha256 is required for tap".into());
    }
    manager()?
        .perform_action(
            &args,
            "tap",
            "/api/tap",
            json!({"x": x, "y": y}),
            false,
            args["settle_ms"].as_u64().unwrap_or(900),
        )
        .await
}

fn active_policy(manager: &IosDeviceManager, args: &Value) -> Result<ControlPolicy, String> {
    let session_id = required_str(args, "session_id")?;
    let state = manager.state.lock().unwrap_or_else(|e| e.into_inner());
    let lease = state.lease.as_ref().ok_or("NO_ACTIVE_LEASE")?;
    if lease.expires_at <= Instant::now() {
        return Err("LEASE_EXPIRED: automatic link cleanup is pending".into());
    }
    if lease.session_id != session_id {
        return Err("LEASE_MISMATCH: session_id does not own the active device lease".into());
    }
    Ok(lease.policy)
}

fn validate_driver_url(value: &str) -> Result<String, String> {
    let mut url = Url::parse(value).map_err(|e| format!("invalid SIRIN_IOS_DRIVER_URL: {e}"))?;
    if url.scheme() != "http" {
        return Err("SIRIN_IOS_DRIVER_URL must use plain HTTP on loopback".into());
    }
    if !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")) {
        return Err("SIRIN_IOS_DRIVER_URL must remain on loopback".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err("SIRIN_IOS_DRIVER_URL must not contain credentials or a query".into());
    }
    url.set_path("");
    url.set_fragment(None);
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn provider_post_timeout(path: &str) -> Duration {
    if matches!(path, "/api/acceptance-route" | "/api/ensure-link") {
        Duration::from_secs(60)
    } else {
        Duration::from_secs(30)
    }
}

fn provider_http_error(method: &str, path: &str, status: StatusCode, body: &[u8]) -> String {
    let message = String::from_utf8_lossy(body);
    let compact: String = message.chars().take(300).collect();
    format!(
        "iOS driver {method} {path} returned HTTP {}: {compact}",
        status.as_u16()
    )
}

fn phone_response_is_readable(value: &Value) -> bool {
    value["info_readable"].as_bool().unwrap_or_else(|| {
        value["session"]
            .as_str()
            .is_some_and(|session| !session.trim().is_empty())
            || value["locked"].is_boolean()
            || value["app"].is_string()
            || value["app"]["bundleId"].is_string()
            || !value["battery"].is_null()
    })
}

fn lease_json(lease: &Lease) -> Value {
    json!({
        "session_id": lease.session_id,
        "owner": lease.owner,
        "policy": lease.policy.as_str(),
        "started_at": lease.started_at,
        "ttl_secs": lease.ttl_secs,
        "expired": lease.expires_at <= Instant::now(),
        "remaining_secs": lease.expires_at.saturating_duration_since(Instant::now()).as_secs(),
        "completed_action_count": lease.completed_actions.len(),
    })
}

fn take_expired_lease(state: &mut ManagerState, expected_id: Option<&str>) -> Option<Lease> {
    let should_release = state.lease.as_ref().is_some_and(|lease| {
        lease.expires_at <= Instant::now()
            && expected_id.map_or(true, |expected| expected == lease.session_id)
    });
    should_release.then(|| state.lease.take()).flatten()
}

fn provider_action_confirmed(value: &Value) -> bool {
    value["ok"].as_bool() == Some(true)
}

fn proof(passed: bool, evidence: &str) -> Value {
    if passed {
        json!({"status": "PASS", "evidence": evidence})
    } else {
        json!({"status": "MISSING_PROOF"})
    }
}

fn human_blockers(provider: &ProviderSnapshot) -> Vec<&'static str> {
    let mut blockers = Vec::new();
    if provider.locked == Some(true) {
        blockers.push("NEEDS_HUMAN_UNLOCK");
    }
    if provider.link.as_deref() != Some("up") || provider.input_reported != Some(true) {
        let blocker = match provider.last_link_result.as_deref() {
            Some("NEEDS_HUMAN_USB") => "NEEDS_HUMAN_USB",
            Some("NEEDS_HUMAN_WAKE_UNLOCK") => "NEEDS_HUMAN_WAKE_UNLOCK",
            Some("NEEDS_HUMAN_DDI") => "NEEDS_HUMAN_DDI",
            Some("NEEDS_HUMAN_WDA_INSTALL") => "NEEDS_HUMAN_WDA_INSTALL",
            Some("NEEDS_HUMAN_WDA_WEDGED") => "NEEDS_HUMAN_WDA_WEDGED",
            Some("SECURITY_BLOCK_LAN_PROTECTION_MISSING") => {
                "SECURITY_BLOCK_LAN_ACTIVATION_UNPROTECTED"
            }
            Some("DRIVER_DEVICE_PROBE_FAILED") => "MAINTENANCE_DRIVER_DEVICE_PROBE_FAILED",
            Some("PASSIVE_LINK_NOT_STARTED") => "",
            _ => "NEEDS_HUMAN_DEVICE_CHECK",
        };
        if !blocker.is_empty() && !blockers.contains(&blocker) {
            blockers.push(blocker);
        }
    }
    if provider.lan_exposed == Some(true) {
        blockers.push("SECURITY_BLOCK_LAN_EXPOSURE");
    } else if provider.lan_exposed.is_none() {
        blockers.push("SECURITY_BLOCK_LAN_STATE_UNKNOWN");
    }
    if provider.link.as_deref() != Some("up")
        && provider.lan_protection.as_deref() == Some("forwards_inactive_activation_blocked")
        && !blockers.contains(&"SECURITY_BLOCK_LAN_ACTIVATION_UNPROTECTED")
    {
        blockers.push("SECURITY_BLOCK_LAN_ACTIVATION_UNPROTECTED");
    }
    blockers
}

fn lan_safety_error(provider: &ProviderSnapshot) -> String {
    if provider.lan_exposed == Some(true) {
        "POLICY_DENIED: iPhone driver forward is exposed to the LAN".into()
    } else {
        format!(
            "MISSING_PROOF: iPhone driver LAN safety is unknown (protection={})",
            provider.lan_protection.as_deref().unwrap_or("unknown")
        )
    }
}

fn link_activation_safety_error(provider: &ProviderSnapshot) -> Option<String> {
    if provider.link.as_deref() == Some("up") && provider.input_reported == Some(true) {
        return None;
    }
    match provider.lan_protection.as_deref() {
        Some(
            "loopback_listener"
            | "firewall_blocked_lan_listener"
            | "forwards_inactive_firewall_ready",
        ) => None,
        Some("forwards_inactive_activation_blocked") => Some(
            "POLICY_DENIED: iPhone link activation requires an existing LAN block; Sirin will not change firewall settings"
                .into(),
        ),
        value => Some(format!(
            "MISSING_PROOF: iPhone link activation safety is not proven (protection={})",
            value.unwrap_or("unknown")
        )),
    }
}

fn link_unready_error(provider: &ProviderSnapshot) -> String {
    let result = provider.last_link_result.as_deref().unwrap_or("UNKNOWN");
    if result.starts_with("NEEDS_HUMAN_") {
        format!("{result}: WDA input link is not currently ready")
    } else {
        format!("MISSING_PROOF: WDA input link is not currently ready (last_link_result={result})")
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing {key}"))
}

fn required_f64(args: &Value, key: &str) -> Result<f64, String> {
    let value = args[key]
        .as_f64()
        .ok_or_else(|| format!("missing or invalid {key}"))?;
    if !value.is_finite() || value < 0.0 || value > 10_000.0 {
        return Err(format!("{key} is outside the supported coordinate range"));
    }
    Ok(value)
}

fn validate_action_id(value: &str) -> Result<String, String> {
    let valid = !value.is_empty()
        && value.len() <= 120
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        return Err("action_id must be 1-120 ASCII letters, numbers, '.', '_' or '-'".into());
    }
    Ok(value.into())
}

fn sanitize_owner(value: &str) -> String {
    let value: String = value.chars().filter(|c| !c.is_control()).take(80).collect();
    if value.trim().is_empty() {
        "codex".into()
    } else {
        value
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn prove_control_change(
    action: &str,
    before: &CaptureEvidence,
    after: &CaptureEvidence,
) -> Result<(bool, VisualChangeEvidence, String), String> {
    let before_image = image::open(&before.path)
        .map_err(|error| format!("cannot decode before frame '{}': {error}", before.path))?
        .to_luma8();
    let after_image = image::open(&after.path)
        .map_err(|error| format!("cannot decode after frame '{}': {error}", after.path))?
        .to_luma8();
    let require_vertical_scroll = action == "swipe";
    let visual_change = analyze_visual_change(
        &before_image,
        &after_image,
        before.sha256 != after.sha256,
        require_vertical_scroll,
    )?;
    let changed = if require_vertical_scroll {
        visual_change.vertical_scroll_proven == Some(true)
    } else {
        visual_change.material_content_change
    };
    let note = if require_vertical_scroll {
        if changed {
            format!(
                "Content-region comparison excludes status/browser chrome and proves material vertical displacement (changed_pixel_ratio={:.4}, best_vertical_shift_pixels={}, improvement={:.4}).",
                visual_change.changed_pixel_ratio,
                visual_change.best_vertical_shift_pixels.unwrap_or_default(),
                visual_change.vertical_shift_improvement_ratio.unwrap_or_default(),
            )
        } else {
            format!(
                "The provider accepted the swipe, but content-region comparison did not prove material vertical displacement (full_frame_sha256_changed={}, changed_pixel_ratio={:.4}, best_vertical_shift_pixels={}, improvement={:.4}); SCREEN_CONTROL remains unproven.",
                visual_change.full_frame_sha256_changed,
                visual_change.changed_pixel_ratio,
                visual_change.best_vertical_shift_pixels.unwrap_or_default(),
                visual_change.vertical_shift_improvement_ratio.unwrap_or_default(),
            )
        }
    } else if changed {
        format!(
            "Content-region comparison excludes status/browser chrome and proves a material screen change (changed_pixel_ratio={:.4}).",
            visual_change.changed_pixel_ratio,
        )
    } else {
        format!(
            "The provider accepted the action, but no material content-region change was proven (full_frame_sha256_changed={}, changed_pixel_ratio={:.4}); screen control remains unproven.",
            visual_change.full_frame_sha256_changed,
            visual_change.changed_pixel_ratio,
        )
    };
    Ok((changed, visual_change, note))
}

fn analyze_visual_change(
    before: &GrayImage,
    after: &GrayImage,
    full_frame_sha256_changed: bool,
    require_vertical_scroll: bool,
) -> Result<VisualChangeEvidence, String> {
    let (width, height) = before.dimensions();
    if after.dimensions() != (width, height) {
        return Err(format!(
            "frame dimensions differ: before={}x{}, after={}x{}",
            width,
            height,
            after.width(),
            after.height()
        ));
    }
    if width < MIN_SCREEN_EDGE || height < MIN_SCREEN_EDGE {
        return Err(format!(
            "frame is too small for content analysis: {width}x{height}"
        ));
    }

    let x = width.saturating_mul(CONTENT_LEFT_PERCENT) / 100;
    let y = height.saturating_mul(CONTENT_TOP_PERCENT) / 100;
    let right = width.saturating_mul(CONTENT_RIGHT_PERCENT) / 100;
    let bottom = height.saturating_mul(CONTENT_BOTTOM_PERCENT) / 100;
    if right <= x || bottom <= y {
        return Err("content region is empty".into());
    }
    let region = ContentRegionEvidence {
        x,
        y,
        width: right - x,
        height: bottom - y,
    };
    let stride = (width.max(height) / 250).max(1);
    let (baseline_mean, changed_pixels, sampled_pixels) =
        compare_content_at_shift(before, after, &region, stride, 0);
    if sampled_pixels == 0 {
        return Err("content region produced no samples".into());
    }
    let changed_pixel_ratio = changed_pixels as f64 / sampled_pixels as f64;
    let material_content_change = changed_pixel_ratio >= MATERIAL_CHANGED_PIXEL_RATIO;

    let mut best_mean = baseline_mean;
    let mut best_shift = 0_i32;
    let max_steps = ((region.height / 2) / stride) as i32;
    for shift_steps in -max_steps..=max_steps {
        if shift_steps.abs() < MIN_VERTICAL_SHIFT_STEPS {
            continue;
        }
        let shift_pixels = shift_steps * stride as i32;
        let (mean, _, samples) =
            compare_content_at_shift(before, after, &region, stride, shift_pixels);
        if samples >= sampled_pixels / 2 && mean < best_mean {
            best_mean = mean;
            best_shift = shift_pixels;
        }
    }
    let improvement = if baseline_mean > f64::EPSILON {
        ((baseline_mean - best_mean) / baseline_mean).max(0.0)
    } else {
        0.0
    };
    let vertical_scroll_proven = material_content_change
        && best_shift.abs() >= MIN_VERTICAL_SHIFT_STEPS * stride as i32
        && improvement >= MIN_VERTICAL_SHIFT_IMPROVEMENT;

    Ok(VisualChangeEvidence {
        full_frame_sha256_changed,
        content_region: region,
        sample_stride_pixels: stride,
        sampled_pixels,
        changed_pixel_ratio: round_six(changed_pixel_ratio),
        mean_absolute_delta_0_to_255: round_six(baseline_mean),
        material_content_change,
        best_vertical_shift_pixels: require_vertical_scroll.then_some(best_shift),
        vertical_shift_improvement_ratio: require_vertical_scroll.then_some(round_six(improvement)),
        vertical_scroll_proven: require_vertical_scroll.then_some(vertical_scroll_proven),
    })
}

fn compare_content_at_shift(
    before: &GrayImage,
    after: &GrayImage,
    region: &ContentRegionEvidence,
    stride: u32,
    vertical_shift_pixels: i32,
) -> (f64, u64, u64) {
    let mut absolute_delta_sum = 0_u64;
    let mut changed_pixels = 0_u64;
    let mut sampled_pixels = 0_u64;
    let right = region.x + region.width;
    let bottom = region.y + region.height;
    for before_y in (region.y..bottom).step_by(stride as usize) {
        let after_y = before_y as i64 + vertical_shift_pixels as i64;
        if after_y < region.y as i64 || after_y >= bottom as i64 {
            continue;
        }
        for x in (region.x..right).step_by(stride as usize) {
            let before_value = before.get_pixel(x, before_y)[0];
            let after_value = after.get_pixel(x, after_y as u32)[0];
            let delta = before_value.abs_diff(after_value);
            absolute_delta_sum += u64::from(delta);
            changed_pixels += u64::from(delta >= CONTENT_PIXEL_DELTA_THRESHOLD);
            sampled_pixels += 1;
        }
    }
    let mean = if sampled_pixels == 0 {
        f64::INFINITY
    } else {
        absolute_delta_sum as f64 / sampled_pixels as f64
    };
    (mean, changed_pixels, sampled_pixels)
}

fn round_six(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn sanitize_file_part(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    if value.trim_matches('_').is_empty() {
        "capture".into()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_response_driver(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake driver");
        let address = listener.local_addr().expect("fake driver address");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept fake driver request");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("read timeout");
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).expect("read fake driver request");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write fake driver response");
            request
        });
        (format!("http://{address}"), handle)
    }

    fn scripted_driver(
        bodies: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted driver");
        let address = listener.local_addr().expect("scripted driver address");
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept scripted request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("read timeout");
                let mut buffer = [0_u8; 4096];
                let read = stream.read(&mut buffer).expect("read scripted request");
                requests.push(String::from_utf8_lossy(&buffer[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write scripted response");
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn driver_url_must_stay_on_loopback() {
        assert_eq!(
            validate_driver_url("http://127.0.0.1:8770/").expect("loopback"),
            "http://127.0.0.1:8770"
        );
        assert!(validate_driver_url("http://192.168.1.20:8770").is_err());
        assert!(validate_driver_url("https://127.0.0.1:8770").is_err());
        assert!(validate_driver_url("http://user:secret@127.0.0.1:8770").is_err());
    }

    #[test]
    fn acceptance_route_allows_bounded_post_navigation_verification() {
        assert_eq!(
            provider_post_timeout("/api/acceptance-route"),
            Duration::from_secs(60)
        );
        assert_eq!(
            provider_post_timeout("/api/ensure-link"),
            Duration::from_secs(60)
        );
        assert_eq!(provider_post_timeout("/api/tap"), Duration::from_secs(30));
    }

    #[test]
    fn policies_fail_closed() {
        assert_eq!(
            ControlPolicy::parse(None).expect("default"),
            ControlPolicy::AcceptanceBrowseOnly
        );
        assert!(ControlPolicy::parse(Some("free_control")).is_err());
    }

    #[test]
    fn acceptance_routes_are_fixed_and_include_codex_remote() {
        assert!(is_allowed_acceptance_route("safari_store"));
        assert!(is_allowed_acceptance_route("telegram_bot"));
        assert!(is_allowed_acceptance_route("codex_remote"));
        assert!(!is_allowed_acceptance_route(
            "https://remote.purrtechllc.com/"
        ));
        assert!(!is_allowed_acceptance_route("arbitrary"));
    }

    #[test]
    fn action_ids_and_coordinates_are_bounded() {
        assert_eq!(
            validate_action_id("run-1.swipe_2").expect("id"),
            "run-1.swipe_2"
        );
        assert!(validate_action_id("contains space").is_err());
        assert!(required_f64(&json!({"x": -1}), "x").is_err());
        assert!(required_f64(&json!({"x": 99}), "x").is_ok());
    }

    #[test]
    fn expired_lease_is_taken_for_link_cleanup() {
        let mut state = ManagerState {
            lease: Some(Lease {
                session_id: "expired".into(),
                owner: "test".into(),
                policy: ControlPolicy::AcceptanceBrowseOnly,
                started_at: Utc::now().to_rfc3339(),
                expires_at: Instant::now() - Duration::from_secs(1),
                ttl_secs: 30,
                completed_actions: BTreeMap::new(),
                action_order: VecDeque::new(),
            }),
            ..ManagerState::default()
        };
        let expired = take_expired_lease(&mut state, Some("expired"));
        assert_eq!(
            expired.map(|lease| lease.session_id).as_deref(),
            Some("expired")
        );
        assert!(state.lease.is_none());
    }

    #[tokio::test]
    async fn expired_lease_posts_verified_link_release() {
        let (url, server) = one_response_driver(
            r#"{"ok":true,"status":"released","forward_listener_scope":"inactive"}"#,
        );
        let manager = IosDeviceManager::new(&url).expect("manager");
        manager
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lease = Some(Lease {
            session_id: "expired-http".into(),
            owner: "test".into(),
            policy: ControlPolicy::AcceptanceBrowseOnly,
            started_at: Utc::now().to_rfc3339(),
            expires_at: Instant::now() - Duration::from_secs(1),
            ttl_secs: 30,
            completed_actions: BTreeMap::new(),
            action_order: VecDeque::new(),
        });

        let _action_guard = manager.action_gate.lock().await;
        let release = manager
            .release_expired_lease_locked(Some("expired-http"))
            .await
            .expect("expired release result");

        assert_eq!(release["ok"], true);
        let state = manager.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(state.lease.is_none());
        assert!(!state.link_cleanup_required);
        drop(state);
        let request = server.join().expect("fake driver thread");
        assert!(request.starts_with("POST /api/release-link "));
    }

    #[tokio::test]
    async fn failed_start_releases_a_preexisting_provider_link() {
        let snapshot = r#"{
            "status": {
                "provider": "sirin-ios-driver",
                "device_detected": true,
                "device_probe_status": "ok",
                "acceptance_only": true,
                "input": true,
                "link": "up",
                "starting": false,
                "last_link_result": "HEALTHY",
                "lan_exposed": false,
                "lan_protection": "firewall_blocked_lan_listener",
                "attach_mode": "passive"
            },
            "phone_status": 200,
            "phone": {
                "info_readable": true,
                "locked": true,
                "app": "com.apple.mobilesafari"
            }
        }"#;
        let released = r#"{"ok":true,"status":"released","forward_listener_scope":"inactive"}"#;
        let (url, server) = scripted_driver(vec![snapshot, released]);
        let manager = IosDeviceManager::new(&url).expect("manager");

        let error = manager
            .start_session(json!({
                "owner": "test",
                "policy": "acceptance_browse_only",
                "ttl_secs": 30
            }))
            .await
            .expect_err("locked phone must reject the lease");

        assert!(error.starts_with("NEEDS_HUMAN_UNLOCK:"));
        let state = manager.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(state.lease.is_none());
        assert!(!state.link_cleanup_required);
        assert_eq!(
            state
                .last_link_cleanup
                .as_ref()
                .and_then(|v| v["reason"].as_str()),
            Some("session_start_failed")
        );
        drop(state);
        let requests = server.join().expect("scripted driver thread");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /api/snapshot "));
        assert!(requests[1].starts_with("POST /api/release-link "));
    }

    #[tokio::test]
    async fn public_stop_rejects_retaining_a_warm_link() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        let error = manager
            .stop_session(json!({"session_id": "any", "release_link": false}))
            .await
            .expect_err("warm-link bypass must be denied");
        assert!(error.starts_with("POLICY_DENIED:"));
    }

    #[test]
    fn provider_health_requires_current_passive_safety_contract() {
        assert_eq!(
            classify_provider_healthz(&json!({
                "ok": true,
                "provider": "sirin-ios-driver",
                "acceptance_only": true,
                "attach_mode": "passive",
                "bind": "127.0.0.1"
            })),
            ProviderHealth::Healthy
        );
        assert_eq!(
            classify_provider_healthz(&json!({
                "ok": true,
                "provider": "sirin-ios-driver",
                "acceptance_only": true
            })),
            ProviderHealth::Stale("STALE_PROVIDER_SAFETY_CONTRACT")
        );
    }

    #[test]
    fn provider_false_result_never_confirms_an_action() {
        assert!(!provider_action_confirmed(&json!({
            "ok": false,
            "state": "NEEDS_HUMAN_BOT_START"
        })));
        assert!(!provider_action_confirmed(&json!({"state": "PASS"})));
        assert!(provider_action_confirmed(&json!({"ok": true})));
    }

    #[test]
    fn acceptance_run_id_rejects_a_different_route() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        manager.remember_acceptance_run(
            "same-run",
            json!({"ok": true, "run_id": "same-run", "route": "safari_store"}),
        );
        assert!(manager
            .acceptance_run_result("same-run", "safari_store")
            .expect("same route")
            .is_some());
        assert!(manager
            .acceptance_run_result("same-run", "codex_remote")
            .expect_err("different route must conflict")
            .starts_with("IDEMPOTENCY_CONFLICT:"));
    }

    #[test]
    fn acceptance_provider_contract_rejects_legacy_or_unreadable_state() {
        let valid = ProviderSnapshot {
            reachable: true,
            provider_name: "sirin-ios-driver".into(),
            acceptance_only: Some(true),
            attach_mode: Some("passive".into()),
            ..ProviderSnapshot::default()
        };
        assert!(validate_provider_contract(&valid, ControlPolicy::AcceptanceBrowseOnly).is_ok());
        assert!(passive_provider_contract_ok(&valid));

        let legacy = ProviderSnapshot {
            attach_mode: None,
            ..valid
        };
        assert!(
            validate_provider_contract(&legacy, ControlPolicy::AcceptanceBrowseOnly)
                .expect_err("legacy provider must fail")
                .starts_with("POLICY_DENIED:")
        );
        assert!(!passive_provider_contract_ok(&legacy));
    }

    #[test]
    fn screenshot_hash_is_stable() {
        assert_eq!(
            sha256_hex(b"sirin"),
            "1dc3c45da5c876a82e5b7232822da29c2b67fbb3009d308431f56b5f3007c949"
        );
    }

    #[test]
    fn status_and_browser_chrome_changes_do_not_prove_control() {
        let before = GrayImage::from_pixel(200, 400, image::Luma([80]));
        let mut after = before.clone();
        for y in 0..32 {
            for x in 0..200 {
                after.put_pixel(x, y, image::Luma([180]));
            }
        }
        for y in 340..400 {
            for x in 0..200 {
                after.put_pixel(x, y, image::Luma([220]));
            }
        }

        let evidence = analyze_visual_change(&before, &after, true, true).expect("visual evidence");
        assert!(evidence.full_frame_sha256_changed);
        assert_eq!(evidence.changed_pixel_ratio, 0.0);
        assert!(!evidence.material_content_change);
        assert_eq!(evidence.vertical_scroll_proven, Some(false));
    }

    #[test]
    fn content_vertical_displacement_proves_swipe_control() {
        let mut before = GrayImage::new(200, 400);
        for y in 0..400 {
            for x in 0..200 {
                let value = ((x * 17 + y * 31 + (x * y) % 251) % 256) as u8;
                before.put_pixel(x, y, image::Luma([value]));
            }
        }
        let mut after = GrayImage::from_pixel(200, 400, image::Luma([0]));
        for y in 60..400 {
            for x in 0..200 {
                after.put_pixel(x, y, *before.get_pixel(x, y - 60));
            }
        }

        let evidence = analyze_visual_change(&before, &after, true, true).expect("visual evidence");
        assert!(evidence.material_content_change);
        assert_eq!(evidence.best_vertical_shift_pixels, Some(60));
        assert!(
            evidence
                .vertical_shift_improvement_ratio
                .unwrap_or_default()
                > 0.9
        );
        assert_eq!(evidence.vertical_scroll_proven, Some(true));
    }

    #[test]
    fn wholesale_content_change_is_not_misclassified_as_vertical_swipe() {
        let before = GrayImage::from_pixel(200, 400, image::Luma([30]));
        let after = GrayImage::from_pixel(200, 400, image::Luma([220]));

        let evidence = analyze_visual_change(&before, &after, true, true).expect("visual evidence");
        assert!(evidence.material_content_change);
        assert_eq!(evidence.best_vertical_shift_pixels, Some(0));
        assert_eq!(evidence.vertical_shift_improvement_ratio, Some(0.0));
        assert_eq!(evidence.vertical_scroll_proven, Some(false));
    }

    #[test]
    fn lifecycle_status_is_sirin_owned_and_fail_closed_by_default() {
        let status = driver_lifecycle_json();
        assert_eq!(status["owner"], "sirin");
        assert_eq!(status["provider_private"], true);
        assert!(status["repair_scope"]
            .as_str()
            .expect("repair scope")
            .contains("no stop"));
    }

    #[test]
    fn scheduled_daemon_flag_enables_start_only_driver_supervision() {
        let args = vec![
            "sirin.exe".to_string(),
            "--ios-driver-autostart".to_string(),
        ];
        assert!(driver_autostart_requested(&args, None));
        assert!(driver_autostart_requested(
            &["sirin.exe".to_string()],
            Some("true")
        ));
        assert!(!driver_autostart_requested(
            &["sirin.exe".to_string()],
            Some("0")
        ));
    }

    #[test]
    fn empty_phone_payload_never_proves_readable_information() {
        assert!(!phone_response_is_readable(&json!({})));
        assert!(!phone_response_is_readable(&json!({
            "info_readable": false,
            "session": "stale-session",
            "battery": {"level": 1.0}
        })));
        assert!(phone_response_is_readable(&json!({
            "info_readable": true,
            "session": "fresh-session"
        })));
        assert!(phone_response_is_readable(&json!({
            "session": "legacy-live-session"
        })));
    }

    #[test]
    fn provider_link_result_preserves_exact_human_boundary() {
        let mut provider = ProviderSnapshot {
            input_reported: Some(false),
            link: Some("down".into()),
            last_link_result: Some("NEEDS_HUMAN_WAKE_UNLOCK".into()),
            lan_exposed: Some(false),
            lan_protection: Some("forwards_inactive_firewall_ready".into()),
            ..ProviderSnapshot::default()
        };
        assert_eq!(human_blockers(&provider), vec!["NEEDS_HUMAN_WAKE_UNLOCK"]);
        assert!(link_unready_error(&provider).starts_with("NEEDS_HUMAN_WAKE_UNLOCK:"));

        provider.last_link_result = Some("NEEDS_HUMAN_WDA_INSTALL".into());
        assert_eq!(human_blockers(&provider), vec!["NEEDS_HUMAN_WDA_INSTALL"]);
        assert!(link_unready_error(&provider).starts_with("NEEDS_HUMAN_WDA_INSTALL:"));
    }

    #[test]
    fn provider_link_timeout_remains_missing_proof_not_false_unlock_claim() {
        let provider = ProviderSnapshot {
            input_reported: Some(false),
            link: Some("down".into()),
            last_link_result: Some("WDA_START_TIMEOUT".into()),
            lan_exposed: Some(false),
            lan_protection: Some("forwards_inactive_firewall_ready".into()),
            ..ProviderSnapshot::default()
        };
        assert_eq!(human_blockers(&provider), vec!["NEEDS_HUMAN_DEVICE_CHECK"]);
        assert!(link_unready_error(&provider).contains("last_link_result=WDA_START_TIMEOUT"));
    }

    #[test]
    fn passive_link_is_not_misreported_as_a_human_blocker() {
        let provider = ProviderSnapshot {
            device_detected_reported: Some(true),
            input_reported: Some(false),
            link: Some("down".into()),
            last_link_result: Some("PASSIVE_LINK_NOT_STARTED".into()),
            lan_exposed: Some(false),
            lan_protection: Some("forwards_inactive_firewall_ready".into()),
            ..ProviderSnapshot::default()
        };
        assert!(human_blockers(&provider).is_empty());
    }

    #[test]
    fn inactive_forward_without_existing_lan_block_prevents_link_activation() {
        let provider = ProviderSnapshot {
            device_detected_reported: Some(true),
            input_reported: Some(false),
            link: Some("down".into()),
            last_link_result: Some("PASSIVE_LINK_NOT_STARTED".into()),
            lan_exposed: Some(false),
            lan_protection: Some("forwards_inactive_activation_blocked".into()),
            ..ProviderSnapshot::default()
        };
        assert!(human_blockers(&provider).contains(&"SECURITY_BLOCK_LAN_ACTIVATION_UNPROTECTED"));
        assert!(link_activation_safety_error(&provider)
            .unwrap()
            .starts_with("POLICY_DENIED:"));
    }

    #[test]
    fn unknown_lan_state_fails_closed() {
        let provider = ProviderSnapshot {
            lan_exposed: None,
            lan_protection: Some("listener_check_failed".into()),
            ..ProviderSnapshot::default()
        };
        assert!(human_blockers(&provider).contains(&"SECURITY_BLOCK_LAN_STATE_UNKNOWN"));
        assert!(lan_safety_error(&provider).starts_with("MISSING_PROOF:"));
    }

    fn test_lease(session_id: &str) -> Lease {
        Lease {
            session_id: session_id.into(),
            owner: "unit-test".into(),
            policy: ControlPolicy::AcceptanceBrowseOnly,
            started_at: Utc::now().to_rfc3339(),
            expires_at: Instant::now() + Duration::from_secs(60),
            ttl_secs: 60,
            completed_actions: BTreeMap::new(),
            action_order: VecDeque::new(),
        }
    }

    #[tokio::test]
    async fn browse_only_policy_rejects_tap_before_contacting_driver() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        manager
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lease = Some(test_lease("browse-only"));

        let error = manager
            .perform_action(
                &json!({
                    "session_id": "browse-only",
                    "action_id": "tap-must-be-denied"
                }),
                "tap",
                "/api/tap",
                json!({"x": 10.0, "y": 10.0}),
                false,
                200,
            )
            .await
            .expect_err("browse-only lease must reject arbitrary taps");
        assert!(error.starts_with("POLICY_DENIED:"));
    }

    #[tokio::test]
    async fn repeated_action_id_replays_result_without_contacting_driver() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        let mut lease = test_lease("idempotent");
        lease
            .completed_actions
            .insert("same-action".into(), json!({"ok": true, "action": "swipe"}));
        manager
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lease = Some(lease);

        let result = manager
            .perform_action(
                &json!({
                    "session_id": "idempotent",
                    "action_id": "same-action"
                }),
                "swipe",
                "/this-path-must-never-be-called",
                json!({}),
                true,
                200,
            )
            .await
            .expect("stored action result");
        assert_eq!(result["idempotent_replay"], true);
        assert_eq!(result["action"], "swipe");
    }

    #[tokio::test]
    async fn repeated_recovery_action_id_replays_without_contacting_driver() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        manager
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recovery_actions
            .insert(
                "same-recovery".into(),
                json!({"ok": true, "action": "recover_home"}),
            );

        let result = manager
            .recover_home(json!({"action_id": "same-recovery"}))
            .await
            .expect("stored recovery result");
        assert_eq!(result["idempotent_replay"], true);
        assert_eq!(result["action"], "recover_home");
    }

    #[tokio::test]
    async fn recovery_is_rejected_while_a_control_lease_is_active() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        manager
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .lease = Some(test_lease("normal-control"));

        let error = manager
            .recover_home(json!({"action_id": "must-not-run"}))
            .await
            .expect_err("recovery must not bypass a normal control lease");
        assert!(error.starts_with("DEVICE_BUSY:"));
    }

    #[tokio::test]
    #[ignore = "requires the local Sirin iOS Driver"]
    async fn live_provider_status_and_capture_are_fresh() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        let provider = manager.provider_snapshot().await;
        assert!(provider.reachable, "Sirin iOS Driver must answer now");
        assert!(provider.device_detected(), "fresh device evidence required");
        assert!(provider.phone_readable, "fresh phone information required");
        let capture = manager
            .capture("live-readonly-proof")
            .await
            .expect("capture");
        assert!(capture.width >= MIN_SCREEN_EDGE);
        assert!(capture.height >= MIN_SCREEN_EDGE);
        assert!(std::path::Path::new(&capture.path).is_file());
    }

    #[tokio::test]
    #[ignore = "requires the healthy local iPhone provider; performs no phone action"]
    async fn live_driver_manager_tick_is_read_only_when_provider_is_healthy() {
        let before = driver_lifecycle_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .start_attempts;
        driver_manager_tick().await.expect("healthy lifecycle tick");
        let state = driver_lifecycle_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(state.start_attempts, before);
        assert_eq!(state.last_result.as_deref(), Some("HEALTHY"));
    }

    #[tokio::test]
    #[ignore = "moves the connected iPhone with one browse-only vertical swipe"]
    async fn live_browse_only_swipe_proves_screen_control() {
        let manager = IosDeviceManager::new(DEFAULT_DRIVER_URL).expect("manager");
        let started = manager
            .start_session(json!({
                "owner": "sirin-live-test",
                "policy": "acceptance_browse_only",
                "ttl_secs": 60
            }))
            .await
            .expect("lease");
        let session_id = started["session_id"].as_str().expect("session id");
        let result = manager
            .perform_action(
                &json!({
                    "session_id": session_id,
                    "action_id": "live-readonly-swipe-1"
                }),
                "swipe",
                "/api/swipe",
                json!({"x1": 195.0, "y1": 650.0, "x2": 195.0, "y2": 250.0, "seconds": 0.3}),
                true,
                1_200,
            )
            .await
            .expect("safe swipe result");
        assert_eq!(result["screen_control"]["status"], "PASS");
        manager
            .stop_session(json!({"session_id": session_id}))
            .await
            .expect("release lease");
    }
}
