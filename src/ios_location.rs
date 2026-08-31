use crate::platform::NoWindow;
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

const DEFAULT_3UTOOLS_LAUNCHER: &str = r"C:\Program Files\3uTools9\3uLauncher.exe";
const DEFAULT_3UTOOLS_EXE: &str = r"C:\Program Files\3uTools9\3uTools.exe";
const DEFAULT_3UTOOLS_DIR: &str = r"C:\Program Files\3uTools9";
const EARTH_RADIUS_M: f64 = 6_371_000.0;
const DEFAULT_ROUTE_SPEED_KMH: f64 = 20.0;
const DEFAULT_ROUTE_DURATION_SECONDS: u64 = 600;
const DEFAULT_ROUTE_TICK_SECONDS: u64 = 1;
const DEFAULT_ROAM_RADIUS_M: f64 = 1500.0;
const DEFAULT_ROAM_WAYPOINTS: u64 = 8;
const DEFAULT_ROAD_PROVIDER: &str = "osrm";
const DEFAULT_OSRM_BASE_URL: &str = "https://router.project-osrm.org";
const MAX_ACCEPTABLE_DETOUR_RATIO: f64 = 3.0;
const MAX_ROUTE_BACKTRACK_COUNT: usize = 0;
const MAX_ROUTE_REPEATED_SEGMENT_COUNT: usize = 0;
const ROUTE_LOOP_SEGMENT_MIN_M: f64 = 8.0;
const ROUTE_BACKTRACK_ANGLE_DEG: f64 = 150.0;
const MAX_ROUTE_STEP_SPEED_KMH: f64 = 20.3;
const MAX_ROUTE_PLANS_IN_MEMORY: usize = 10;
const PLAYBACK_STALL_SECONDS: u64 = 15;
const ROUTE_SPEED_VARIATION_FACTORS: [f64; 10] = [
    0.985, 1.010, 0.995, 1.020, 0.990, 1.005, 0.980, 1.015, 1.000, 1.000,
];
const BICYCLE_SPEED_VARIATION_FACTORS: [f64; 12] = [
    0.92, 1.02, 0.96, 1.08, 0.94, 1.00, 0.88, 1.04, 0.98, 1.06, 0.90, 1.00,
];
const BICYCLE_TURN_LOOKAHEAD_M: f64 = 26.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IosPoint {
    lat: f64,
    lng: f64,
}

#[derive(Debug, Clone, Serialize)]
struct IosLocationSession {
    session_id: String,
    status: String,
    created_at: String,
    updated_at: String,
    mode: String,
    point: Option<IosPoint>,
    target_app: Option<String>,
    operator_action: String,
    evidence_path: String,
    note: Option<String>,
    backend: Option<String>,
    command_summary: Option<String>,
    command_status: Option<String>,
    stdout_excerpt: Option<String>,
    stderr_excerpt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct IosRoutePlan {
    route_id: String,
    status: String,
    created_at: String,
    mode: String,
    city: Option<String>,
    center: Option<IosPoint>,
    radius_m: Option<f64>,
    route_source: String,
    road_provider: Option<String>,
    road_routed: bool,
    movement_profile: String,
    speed_kmh: f64,
    duration_seconds: u64,
    tick_seconds: u64,
    requested_distance_m: f64,
    route_distance_m: f64,
    detour_ratio: Option<f64>,
    route_quality: String,
    route_playable: bool,
    route_block_reason: Option<String>,
    route_loop_metrics: RouteLoopMetrics,
    snap_offset_m: Option<f64>,
    seed_waypoints: Vec<IosPoint>,
    road_route_points: Vec<IosPoint>,
    scheduled_points: Vec<IosPoint>,
    evidence_path: String,
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct RouteLoopMetrics {
    backtrack_count: usize,
    repeated_segment_count: usize,
    overlap_ratio: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoutePlaybackAnalysis {
    quality: String,
    observed_distance_m: f64,
    expected_distance_m: Option<f64>,
    average_speed_kmh: Option<f64>,
    max_step_speed_kmh: Option<f64>,
    overspeed_count: usize,
    stop_count: usize,
    backtrack_count: usize,
    repeated_segment_count: usize,
    overlap_ratio: f64,
    sample_count: usize,
    completed: bool,
    stalled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IosLocationReport {
    report_id: String,
    status: String,
    lat: f64,
    lng: f64,
    accuracy_meters: Option<f64>,
    source: String,
    user_agent: Option<String>,
    reported_at: Option<String>,
    received_at: String,
    evidence_path: String,
    note: Option<String>,
}

static IOS_LOCATION_SESSIONS: OnceLock<Mutex<HashMap<String, IosLocationSession>>> =
    OnceLock::new();
static IOS_ROUTE_PLANS: OnceLock<Mutex<HashMap<String, IosRoutePlan>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, IosLocationSession>> {
    IOS_LOCATION_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn route_plans() -> &'static Mutex<HashMap<String, IosRoutePlan>> {
    IOS_ROUTE_PLANS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn call_ios_3utools_status(_args: Value) -> Result<Value, String> {
    let install = detect_3utools_install();
    let device_cache = device_cache_summary(install.install_dir.as_ref());
    Ok(json!({
        "status": if install.available { "AVAILABLE" } else { "NOT_FOUND" },
        "driver": "ios_3utools_gui",
        "deprecated": true,
        "replacement": "ios_dev_location_status",
        "threeUTools": install,
        "processes": {
            "launcherRunning": process_running("3uLauncher.exe"),
            "appRunning": process_running("3uTools.exe"),
            "viewerRunning": process_running("3uViewer.exe")
        },
        "appleMobileDeviceService": apple_mobile_device_service_status(),
        "deviceCache": device_cache,
        "capabilities": {
            "singlePointPrepare": true,
            "restorePrepare": true,
            "guiAutomation": "planned",
            "roadRouteLowFrequency": "planned_after_single_point_proof",
            "oneSecondTick": false
        },
        "boundaries": {
            "requiresUserOwnedDevice": true,
            "requiresUnlockedTrustedDevice": true,
            "doesNotStoreAppleIdOrPassword": true,
            "doesNotBypassPlatformRules": true,
            "uses3uToolsGuiNotPrivateProtocol": true
        }
    }))
}

pub(crate) fn call_ios_3utools_start(_args: Value) -> Result<Value, String> {
    let install = detect_3utools_install();
    if !install.available {
        return Err("3uTools is not installed at the known paths".into());
    }
    if process_running("3uTools.exe") || process_running("3uLauncher.exe") {
        return Ok(json!({
            "status": "already_running",
            "deprecated": true,
            "replacement": "ios_dev_location_status",
            "threeUTools": install,
            "operatorAction": "Bring 3uTools to the front, connect and unlock the iPhone, then open Toolbox > Virtual Location."
        }));
    }
    let launcher = install
        .launcher_path
        .as_ref()
        .ok_or_else(|| "3uTools launcher path missing".to_string())?;
    let mut command = Command::new(launcher);
    command.no_window();
    if let Some(dir) = &install.install_dir {
        command.current_dir(dir);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start 3uTools: {e}"))?;
    Ok(json!({
        "status": "started",
        "deprecated": true,
        "replacement": "ios_dev_location_status",
        "threeUTools": install,
        "operatorAction": "Connect and unlock the iPhone, trust this computer if prompted, then open Toolbox > Virtual Location."
    }))
}

pub(crate) fn call_ios_dev_location_status(_args: Value) -> Result<Value, String> {
    let pymobiledevice3 = cli_status("pymobiledevice3");
    let idevicesetlocation = cli_status("idevicesetlocation");
    let idevice_id = cli_status("idevice_id");
    Ok(json!({
        "status": if pymobiledevice3.available || idevicesetlocation.available {
            "BACKEND_AVAILABLE"
        } else {
            "BACKEND_MISSING"
        },
        "driver": "ios_developer_location",
        "preferredBackend": select_backend_name("auto"),
        "backends": {
            "pymobiledevice3": pymobiledevice3,
            "libimobiledevice": {
                "idevicesetlocation": idevicesetlocation,
                "ideviceId": idevice_id
            }
        },
        "appleMobileDeviceService": apple_mobile_device_service_status(),
        "connectedDevices": redacted_connected_device_summary(),
        "capabilities": {
            "singlePointSet": pymobiledevice3.available || idevicesetlocation.available,
            "resetLocation": pymobiledevice3.available || idevicesetlocation.available,
            "routeLowFrequency": "planned_after_single_point_proof",
            "requiresDeveloperImageOrDeveloperMode": true,
            "oneSecondTick": false
        },
        "boundaries": {
            "doesNotUse3uTools": true,
            "requiresUserOwnedDevice": true,
            "requiresUnlockedTrustedDevice": true,
            "doesNotStoreAppleIdOrPassword": true,
            "usesDeveloperLocationSimulationService": true
        },
        "installHints": install_hints()
    }))
}

pub(crate) fn call_ios_dev_location_current(args: Value) -> Result<Value, String> {
    let request_id = arg_text(&args, "requestId")
        .or_else(|| arg_text(&args, "request_id"))
        .unwrap_or_else(|| format!("IOSCURRENT-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let mut probes = Vec::new();
    let mut supports_direct_read = false;

    if let Some(command) = find_command("pymobiledevice3") {
        probes.push(json!({
            "source": "pymobiledevice3 --help",
            "available": true,
            "finding": "Top-level commands include simulate-location but no documented current GPS read command."
        }));
        match run_command(
            &command,
            &["developer", "dvt", "device-information", "--userspace"],
        ) {
            Ok(result) => {
                let combined = format!("{}\n{}", result.stdout, result.stderr);
                let location_mentions = text_location_mentions(&combined);
                supports_direct_read |= location_mentions.iter().any(|m| {
                    m.to_ascii_lowercase().contains("latitude")
                        || m.to_ascii_lowercase().contains("longitude")
                });
                probes.push(json!({
                    "source": "pymobiledevice3 developer dvt device-information --userspace",
                    "success": result.success,
                    "locationMentions": location_mentions,
                    "stdoutExcerpt": excerpt(&result.stdout, 1800),
                    "stderrExcerpt": excerpt(&result.stderr, 1200)
                }));
            }
            Err(e) => probes.push(json!({
                "source": "pymobiledevice3 developer dvt device-information --userspace",
                "success": false,
                "error": e
            })),
        }
        match run_command(&command, &["diagnostics", "info"]) {
            Ok(result) => {
                let combined = format!("{}\n{}", result.stdout, result.stderr);
                let location_mentions = text_location_mentions(&combined);
                probes.push(json!({
                    "source": "pymobiledevice3 diagnostics info",
                    "success": result.success,
                    "locationMentions": location_mentions,
                    "stdoutExcerpt": excerpt(&result.stdout, 1200),
                    "stderrExcerpt": excerpt(&result.stderr, 1200)
                }));
            }
            Err(e) => probes.push(json!({
                "source": "pymobiledevice3 diagnostics info",
                "success": false,
                "error": e
            })),
        }
    } else {
        probes.push(json!({
            "source": "pymobiledevice3",
            "available": false,
            "finding": "pymobiledevice3 not found on PATH"
        }));
    }

    let status = if supports_direct_read {
        "UNSUPPORTED_PROBE_FOUND_LOCATION_FIELDS"
    } else {
        "UNAVAILABLE_REQUIRES_APP_REPORT"
    };
    let evidence_path = current_location_evidence_file_path(&request_id);
    let response = json!({
        "status": status,
        "requestId": request_id,
        "driver": "ios_developer_location",
        "directReadAvailable": false,
        "point": Value::Null,
        "probes": probes,
        "evidencePath": evidence_path.display().to_string(),
        "nextAction": "iOS developer/lockdown services exposed to Sirin can set or clear simulated location, but do not expose the device's current GPS coordinate. Report location from a trusted test app/web page, then pass it as center/currentLocation to ios_dev_location_roam_plan.",
        "appReportContract": {
            "lat": "number",
            "lng": "number",
            "accuracyMeters": "optional number",
            "source": "ios_app_or_web_geolocation",
            "reportedAt": "ISO-8601 timestamp"
        }
    });
    write_current_location_evidence(&evidence_path, &response)?;
    Ok(response)
}

pub(crate) fn call_ios_dev_location_set(args: Value) -> Result<Value, String> {
    let point = point_from_args(&args)?;
    let requested_backend = arg_text(&args, "backend").unwrap_or_else(|| "auto".into());
    let backend = select_backend(&requested_backend)?;
    let session_id = arg_text(&args, "sessionId")
        .or_else(|| arg_text(&args, "session_id"))
        .unwrap_or_else(|| format!("IOSDEVLOC-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let mut session = new_session(
        &session_id,
        "developer_location_set",
        Some(point.clone()),
        "Sirin executed iOS developer location simulation set through a CLI backend.",
        arg_text(&args, "targetApp").or_else(|| arg_text(&args, "target_app")),
        arg_text(&args, "note"),
    );
    session.backend = Some(backend.name.clone());
    session.command_summary = Some(backend.set_summary(&point));
    write_session_evidence(&session, "dev_location_set_started")?;
    let result = backend.run_set(&point)?;
    apply_command_result(&mut session, result);
    write_session_evidence(&session, "dev_location_set_finished")?;
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.clone(), session.clone());
    Ok(json!({
        "status": session.status,
        "sessionId": session_id,
        "driver": "ios_developer_location",
        "backend": session.backend,
        "point": point,
        "commandStatus": session.command_status,
        "stdoutExcerpt": session.stdout_excerpt,
        "stderrExcerpt": session.stderr_excerpt,
        "evidencePath": session.evidence_path,
        "nextAction": if session.status == "APPLIED" {
            "Verify the target iOS app or Maps sees the expected location; call ios_dev_location_reset when finished."
        } else if session.status == "NEEDS_OPERATOR_VERIFICATION" {
            "Check the iPhone Maps or target app now. If the location changed to the requested point, call ios_dev_location_reset after verification; otherwise retry with the iPhone unlocked."
        } else {
            "Connect and unlock an owned/test iPhone, trust this computer, then retry ios_dev_location_set."
        }
    }))
}

pub(crate) fn call_ios_dev_location_reset(args: Value) -> Result<Value, String> {
    let requested_backend = arg_text(&args, "backend").unwrap_or_else(|| "auto".into());
    let backend = select_backend(&requested_backend)?;
    let session_id = arg_text(&args, "sessionId")
        .or_else(|| arg_text(&args, "session_id"))
        .unwrap_or_else(|| format!("IOSDEVRESET-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let mut session = new_session(
        &session_id,
        "developer_location_reset",
        None,
        "Sirin executed iOS developer location simulation reset through a CLI backend.",
        None,
        arg_text(&args, "note"),
    );
    session.backend = Some(backend.name.clone());
    session.command_summary = Some(backend.reset_summary());
    write_session_evidence(&session, "dev_location_reset_started")?;
    let result = backend.run_reset()?;
    apply_command_result(&mut session, result);
    write_session_evidence(&session, "dev_location_reset_finished")?;
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.clone(), session.clone());
    Ok(json!({
        "status": session.status,
        "sessionId": session_id,
        "driver": "ios_developer_location",
        "backend": session.backend,
        "commandStatus": session.command_status,
        "stdoutExcerpt": session.stdout_excerpt,
        "stderrExcerpt": session.stderr_excerpt,
        "evidencePath": session.evidence_path,
        "nextAction": if session.status == "RESTORED" {
            "Verify the iPhone has returned to true location; reboot the device if developer simulation remains active."
        } else {
            "Connect and unlock the same owned/test iPhone, trust this computer, then retry ios_dev_location_reset."
        }
    }))
}

pub(crate) fn call_ios_dev_location_session_status(args: Value) -> Result<Value, String> {
    call_ios_location_session_status(args)
}

pub(crate) fn call_ios_dev_location_route_plan(args: Value) -> Result<Value, String> {
    let route_input = ios_route_input_from_args(&args)?;
    build_ios_route_plan_response(args, route_input)
}

pub(crate) fn call_ios_dev_location_roam_plan(args: Value) -> Result<Value, String> {
    let route_input = ios_roam_route_from_args(&args)?;
    build_ios_route_plan_response(args, route_input)
}

fn build_ios_route_plan_response(args: Value, route_input: IosRouteInput) -> Result<Value, String> {
    let points = route_input.points;
    let speed_kmh = f64_param(&args, "speedKmh", DEFAULT_ROUTE_SPEED_KMH)
        .or_else(|| f64_param(&args, "speed_kmh", DEFAULT_ROUTE_SPEED_KMH))
        .unwrap_or(DEFAULT_ROUTE_SPEED_KMH)
        .clamp(1.0, 130.0);
    let duration_seconds = u64_param(&args, "durationSeconds", DEFAULT_ROUTE_DURATION_SECONDS)
        .or_else(|| u64_param(&args, "duration_seconds", DEFAULT_ROUTE_DURATION_SECONDS))
        .unwrap_or(DEFAULT_ROUTE_DURATION_SECONDS)
        .clamp(1, 86_400);
    let tick_seconds = u64_param(&args, "tickSeconds", DEFAULT_ROUTE_TICK_SECONDS)
        .or_else(|| u64_param(&args, "tick_seconds", DEFAULT_ROUTE_TICK_SECONDS))
        .unwrap_or(DEFAULT_ROUTE_TICK_SECONDS)
        .clamp(1, 60);
    let route_source = arg_text(&args, "routeSource")
        .or_else(|| arg_text(&args, "route_source"))
        .unwrap_or(route_input.route_source);
    let route_id = arg_text(&args, "routeId")
        .or_else(|| arg_text(&args, "route_id"))
        .unwrap_or_else(|| format!("IOSROUTE-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let movement_profile = movement_profile_from_args(&args);
    let route_distance_m = route_distance_m(&points);
    let requested_distance_m = speed_kmh * 1000.0 / 3600.0 * duration_seconds as f64;
    let detour_ratio = if requested_distance_m > f64::EPSILON {
        Some(route_distance_m / requested_distance_m)
    } else {
        None
    };
    let snap_offset_m = route_input
        .center
        .as_ref()
        .zip(points.first())
        .map(|(center, first)| haversine_m(center, first));
    let route_loop_metrics = route_loop_metrics(&points);
    let route_quality = route_input.route_quality.unwrap_or_else(|| {
        route_quality_label(
            route_input.road_routed,
            detour_ratio,
            snap_offset_m,
            &route_loop_metrics,
        )
        .into()
    });
    let route_gate = route_plan_gate(
        route_distance_m,
        requested_distance_m,
        &route_quality,
        &route_loop_metrics,
    );
    let scheduled_points = sample_route_points(
        &points,
        speed_kmh,
        duration_seconds,
        tick_seconds,
        route_distance_m,
        &movement_profile,
    );
    let plan = IosRoutePlan {
        route_id: route_id.clone(),
        status: route_gate.status.into(),
        created_at: Utc::now().to_rfc3339(),
        mode: route_input.mode,
        city: route_input.city,
        center: route_input.center,
        radius_m: route_input.radius_m,
        route_source,
        road_provider: route_input.road_provider,
        road_routed: route_input.road_routed,
        movement_profile,
        speed_kmh,
        duration_seconds,
        tick_seconds,
        requested_distance_m,
        route_distance_m,
        detour_ratio,
        route_quality,
        route_playable: route_gate.playable,
        route_block_reason: route_gate.block_reason.map(str::to_string),
        route_loop_metrics,
        snap_offset_m,
        seed_waypoints: route_input.seed_waypoints,
        road_route_points: route_input.road_route_points,
        scheduled_points,
        evidence_path: route_evidence_file_path(&route_id).display().to_string(),
        note: arg_text(&args, "note"),
    };
    write_route_evidence(&plan, "route_plan")?;
    {
        let mut guard = route_plans().lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(route_id.clone(), plan.clone());
        prune_route_plans(&mut guard);
    }
    let include_schedule =
        bool_param(&args, "includeSchedule", false) || bool_param(&args, "include_schedule", false);
    Ok(json!({
        "status": plan.status,
        "routeId": route_id,
        "driver": "ios_developer_location",
        "doesNotUse3uTools": true,
        "mode": plan.mode,
        "city": plan.city,
        "center": plan.center,
        "radiusMeters": plan.radius_m,
        "routeSource": plan.route_source,
        "roadFollowing": true,
        "roadProvider": plan.road_provider,
        "roadRouted": plan.road_routed,
        "movementProfile": plan.movement_profile,
        "speedKmh": plan.speed_kmh,
        "durationSeconds": plan.duration_seconds,
        "tickSeconds": plan.tick_seconds,
        "requestedDistanceM": round_meters(plan.requested_distance_m),
        "routeDistanceM": round_meters(plan.route_distance_m),
        "detourRatio": plan.detour_ratio.map(|value| (value * 100.0).round() / 100.0),
        "routeQuality": plan.route_quality,
        "routePlayable": plan.route_playable,
        "routeBlockReason": plan.route_block_reason,
        "routeLoopMetrics": plan.route_loop_metrics,
        "snapOffsetM": plan.snap_offset_m.map(round_meters),
        "scheduledPointCount": plan.scheduled_points.len(),
        "speedCapKmh": MAX_ROUTE_STEP_SPEED_KMH,
        "seedWaypoints": plan.seed_waypoints,
        "roadRoutePoints": plan.road_route_points,
        "preview": route_preview(&plan.scheduled_points),
        "scheduledPoints": if include_schedule { json!(plan.scheduled_points) } else { Value::Null },
        "evidencePath": plan.evidence_path,
        "nextAction": route_plan_next_action(&plan)
    }))
}

pub(crate) fn call_ios_dev_location_route_status(args: Value) -> Result<Value, String> {
    let route_id = arg_text(&args, "routeId").or_else(|| arg_text(&args, "route_id"));
    let guard = route_plans().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = route_id {
        let plan = guard
            .get(&id)
            .ok_or_else(|| format!("iOS developer location route plan not found: {id}"))?;
        return Ok(json!({ "routePlan": plan }));
    }
    let summaries = guard.values().map(route_plan_summary).collect::<Vec<_>>();
    let total_route_plans = summaries.len();
    Ok(json!({
        "routePlans": summaries,
        "totalRoutePlans": total_route_plans,
        "summaryOnly": true,
        "maxRoutePlansInMemory": MAX_ROUTE_PLANS_IN_MEMORY
    }))
}

fn prune_route_plans(plans: &mut HashMap<String, IosRoutePlan>) {
    if plans.len() <= MAX_ROUTE_PLANS_IN_MEMORY {
        return;
    }
    let mut entries = plans
        .iter()
        .map(|(id, plan)| (plan.created_at.clone(), id.clone()))
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let remove_count = plans.len().saturating_sub(MAX_ROUTE_PLANS_IN_MEMORY);
    for (_, id) in entries.into_iter().take(remove_count) {
        plans.remove(&id);
    }
}

fn route_plan_summary(plan: &IosRoutePlan) -> Value {
    json!({
        "routeId": &plan.route_id,
        "status": &plan.status,
        "createdAt": &plan.created_at,
        "mode": &plan.mode,
        "city": &plan.city,
        "radiusMeters": plan.radius_m,
        "routeSource": &plan.route_source,
        "roadProvider": &plan.road_provider,
        "roadRouted": plan.road_routed,
        "movementProfile": &plan.movement_profile,
        "speedKmh": plan.speed_kmh,
        "durationSeconds": plan.duration_seconds,
        "tickSeconds": plan.tick_seconds,
        "requestedDistanceM": round_meters(plan.requested_distance_m),
        "routeDistanceM": round_meters(plan.route_distance_m),
        "detourRatio": plan.detour_ratio.map(|value| (value * 100.0).round() / 100.0),
        "routeQuality": &plan.route_quality,
        "routePlayable": plan.route_playable,
        "routeBlockReason": &plan.route_block_reason,
        "routeLoopMetrics": &plan.route_loop_metrics,
        "snapOffsetM": plan.snap_offset_m.map(round_meters),
        "seedWaypointCount": plan.seed_waypoints.len(),
        "roadRoutePointCount": plan.road_route_points.len(),
        "scheduledPointCount": plan.scheduled_points.len(),
        "evidencePath": &plan.evidence_path,
        "note": &plan.note,
    })
}

pub(crate) fn call_ios_dev_location_route_start(args: Value) -> Result<Value, String> {
    let route_id = arg_text(&args, "routeId")
        .or_else(|| arg_text(&args, "route_id"))
        .ok_or_else(|| "routeId is required".to_string())?;
    let backend =
        select_backend(&arg_text(&args, "backend").unwrap_or_else(|| "pymobiledevice3".into()))?;
    if backend.style != BackendStyle::PyMobileDevice3 {
        return Err(
            "iOS route playback currently requires pymobiledevice3 simulate-location play".into(),
        );
    }
    let plan = route_plans()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&route_id)
        .cloned()
        .ok_or_else(|| format!("iOS developer location route plan not found: {route_id}"))?;
    if plan.scheduled_points.len() < 2 {
        return Err(format!(
            "route {route_id} has fewer than 2 scheduled points"
        ));
    }
    let connected_devices = redacted_connected_device_summary();
    if !connected_devices["available"].as_bool().unwrap_or(false) {
        return Ok(json!({
            "status": "DEVICE_NOT_CONNECTED",
            "driver": "ios_developer_location",
            "doesNotUse3uTools": true,
            "backend": backend.name,
            "routeId": route_id,
            "durationSeconds": plan.duration_seconds,
            "speedKmh": plan.speed_kmh,
            "scheduledPointCount": plan.scheduled_points.len(),
            "connectedDevices": connected_devices,
            "nextAction": "Connect the owned/test iPhone over USB, unlock it, tap Trust This Computer if prompted, then refresh iOS status and start playback again."
        }));
    }

    let userspace = bool_param(&args, "userspace", true);
    let gpx_path = route_gpx_file_path(&route_id);
    write_route_gpx(&plan, &gpx_path)?;
    let run_id = format!("IOSROUTERUN-{}", Utc::now().format("%Y%m%d%H%M%S%3f"));
    let stdout_path = route_run_file_path(&run_id, "stdout.log");
    let stderr_path = route_run_file_path(&run_id, "stderr.log");
    let evidence_path = route_run_file_path(&run_id, "evidence.json");
    if let Some(parent) = stdout_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create route run dir {}: {e}", parent.display()))?;
    }

    let stdout = File::create(&stdout_path).map_err(|e| {
        format!(
            "failed to create route stdout {}: {e}",
            stdout_path.display()
        )
    })?;
    let stderr = File::create(&stderr_path).map_err(|e| {
        format!(
            "failed to create route stderr {}: {e}",
            stderr_path.display()
        )
    })?;
    let mut command = Command::new(&backend.command);
    command.no_window();
    command
        .arg("developer")
        .arg("dvt")
        .arg("simulate-location")
        .arg("play");
    if userspace {
        command.arg("--userspace");
    }
    command
        .arg(&gpx_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let child = command
        .spawn()
        .map_err(|e| format!("failed to start iOS route playback: {e}"))?;
    let pid = child.id();

    let response = json!({
        "status": "ROUTE_PLAYBACK_STARTED",
        "driver": "ios_developer_location",
        "doesNotUse3uTools": true,
        "backend": backend.name,
        "routeId": route_id,
        "routeRunId": run_id,
        "pid": pid,
        "userspace": userspace,
        "durationSeconds": plan.duration_seconds,
        "tickSeconds": plan.tick_seconds,
        "speedKmh": plan.speed_kmh,
        "movementProfile": plan.movement_profile,
        "scheduledPointCount": plan.scheduled_points.len(),
        "gpxPath": gpx_path.display().to_string(),
        "stdoutPath": stdout_path.display().to_string(),
        "stderrPath": stderr_path.display().to_string(),
        "evidencePath": evidence_path.display().to_string(),
        "nextAction": "Watch the iPhone target app or Maps while playback runs. After verification, call ios_dev_location_reset; if the route must be stopped early, terminate the returned pid first."
    });
    write_route_run_evidence(&evidence_path, &response, &plan)?;
    Ok(response)
}

pub(crate) fn call_ios_dev_location_route_run_status(args: Value) -> Result<Value, String> {
    let run_id = arg_text(&args, "routeRunId")
        .or_else(|| arg_text(&args, "route_run_id"))
        .or_else(latest_route_run_id)
        .ok_or_else(|| "routeRunId is required and no route runs exist".to_string())?;
    let run_dir = route_run_dir_path(&run_id);
    let stdout_path = run_dir.join("stdout.log");
    let stderr_path = run_dir.join("stderr.log");
    let evidence_path = run_dir.join("evidence.json");
    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    let points = parse_playback_set_locations(&stderr);
    let moved_distance_m = route_distance_m(&points);
    let evidence = read_json_file(&evidence_path).unwrap_or_else(|| json!({}));
    let response = evidence
        .get("response")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let route_plan = evidence
        .get("routePlan")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let pid = response.get("pid").and_then(Value::as_u64);
    let duration_seconds = response
        .get("durationSeconds")
        .and_then(Value::as_u64)
        .or_else(|| route_plan.get("duration_seconds").and_then(Value::as_u64));
    let requested_distance_m = route_plan
        .get("requested_distance_m")
        .and_then(Value::as_f64);
    let tick_seconds = response
        .get("tickSeconds")
        .and_then(Value::as_u64)
        .or_else(|| route_plan.get("tick_seconds").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_ROUTE_TICK_SECONDS);
    let scheduled_point_count = response
        .get("scheduledPointCount")
        .and_then(Value::as_u64)
        .or_else(|| {
            route_plan
                .get("scheduled_points")
                .and_then(Value::as_array)
                .map(|points| points.len() as u64)
        });
    let elapsed_seconds = points
        .len()
        .saturating_sub(1)
        .saturating_mul(tick_seconds as usize) as u64;
    let remaining_seconds =
        duration_seconds.map(|duration| duration.saturating_sub(elapsed_seconds));
    let still_running = pid.map(process_running_pid);
    let completed = match (duration_seconds, scheduled_point_count, still_running) {
        (_, _, Some(true)) => false,
        (Some(duration), _, _) if elapsed_seconds >= duration => true,
        (_, Some(total), _) if total > 0 && points.len() as u64 >= total => true,
        (_, _, Some(false)) if points.len() > 0 => true,
        _ => false,
    };
    let stderr_stale_seconds = file_stale_seconds(&stderr_path);
    let stalled = playback_stalled(still_running, completed, points.len(), stderr_stale_seconds);
    let playback_analysis = analyze_route_playback(
        &points,
        tick_seconds,
        requested_distance_m,
        completed,
        stalled,
    );
    let status = if stalled {
        "PLAYBACK_STALLED"
    } else if completed {
        "PLAYBACK_COMPLETED"
    } else if points.len() > 1 {
        "MOVEMENT_OBSERVED"
    } else if points.len() == 1 {
        "ONE_LOCATION_SET"
    } else {
        "NO_MOVEMENT_LOGGED"
    };
    Ok(json!({
        "status": status,
        "driver": "ios_developer_location",
        "routeRunId": run_id,
        "routeId": response.get("routeId").and_then(Value::as_str),
        "pid": pid,
        "stillRunning": still_running,
        "completed": completed,
        "stalled": stalled,
        "stderrStaleSeconds": stderr_stale_seconds,
        "stallThresholdSeconds": PLAYBACK_STALL_SECONDS,
        "durationSeconds": duration_seconds,
        "tickSeconds": tick_seconds,
        "requestedDistanceM": requested_distance_m.map(round_meters),
        "scheduledPointCount": scheduled_point_count,
        "elapsedSeconds": elapsed_seconds,
        "remainingSeconds": remaining_seconds,
        "setLocationCount": points.len(),
        "firstPoint": points.first(),
        "lastPoint": points.last(),
        "movedDistanceM": round_meters(moved_distance_m),
        "playbackAnalysis": playback_analysis,
        "gpxPath": response.get("gpxPath").and_then(Value::as_str),
        "stdoutExcerpt": excerpt(&stdout, 1600),
        "stderrExcerpt": excerpt(&stderr, 4000),
        "stdoutPath": stdout_path.display().to_string(),
        "stderrPath": stderr_path.display().to_string(),
        "evidencePath": evidence_path.display().to_string(),
        "evidenceExists": evidence_path.exists(),
        "nextAction": if stalled {
            "Playback process is still alive, but no new set-location log has been written recently. Stop the returned pid, reconnect/unlock the iPhone, run ios_dev_location_reset, then retry playback."
        } else if moved_distance_m >= 100.0 {
            "Sirin observed meaningful movement in the playback log. If the app still looks stationary, refresh/reopen the target app's location view or verify app-side location permissions."
        } else {
            "Movement logged so far is too small to see clearly on a phone map. Use a longer route duration or wait longer before judging movement."
        }
    }))
}

pub(crate) fn call_ios_dev_location_route_stop(args: Value) -> Result<Value, String> {
    let run_id = arg_text(&args, "routeRunId")
        .or_else(|| arg_text(&args, "route_run_id"))
        .or_else(latest_route_run_id);
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .or_else(|| run_id.as_deref().and_then(route_run_pid))
        .ok_or_else(|| "pid is required when no routeRunId evidence is available".to_string())?;

    if !process_running_pid(pid) {
        return Ok(json!({
            "status": "ROUTE_PLAYBACK_NOT_RUNNING",
            "driver": "ios_developer_location",
            "routeRunId": run_id,
            "pid": pid,
            "stillRunning": false,
            "completed": true,
            "nextAction": "No running playback process was found for this pid. If the phone still shows simulated location, call ios_dev_location_reset."
        }));
    }

    let command_result = stop_process_pid(pid)?;
    let still_running = process_running_pid(pid);
    let status = if command_result.success && !still_running {
        "ROUTE_PLAYBACK_STOPPED"
    } else {
        "ROUTE_PLAYBACK_STOP_FAILED"
    };
    Ok(json!({
        "status": status,
        "driver": "ios_developer_location",
        "routeRunId": run_id,
        "pid": pid,
        "commandStatus": if command_result.success { "success" } else { "failed" },
        "stillRunning": still_running,
        "completed": !still_running,
        "stdoutExcerpt": excerpt(&command_result.stdout, 1600),
        "stderrExcerpt": excerpt(&command_result.stderr, 1600),
        "nextAction": if still_running {
            "Playback process is still running. Stop the pid manually, reconnect/unlock the iPhone, then call ios_dev_location_reset."
        } else {
            "Playback process has been stopped. Call ios_dev_location_reset to clear developer simulated location before the next run."
        }
    }))
}

pub(crate) fn call_ios_location_report_latest(_args: Value) -> Result<Value, String> {
    let path = latest_location_report_path();
    let Some(value) = read_json_file(&path) else {
        return Ok(json!({
            "status": "NO_REPORT",
            "driver": "ios_location_report",
            "reportAvailable": false,
            "reportUrl": "http://127.0.0.1:7700/ios-location-report",
            "nextAction": "Open the report URL from the iPhone browser, allow location permission, then tap report."
        }));
    };
    Ok(json!({
        "status": "REPORT_AVAILABLE",
        "driver": "ios_location_report",
        "reportAvailable": true,
        "report": value,
        "point": {
            "lat": value.get("lat").and_then(Value::as_f64),
            "lng": value.get("lng").and_then(Value::as_f64)
        },
        "reportUrl": "http://127.0.0.1:7700/ios-location-report",
        "nextAction": "Use this report point as currentLocation/center before generating the road roam route."
    }))
}

pub(crate) fn ingest_ios_location_report(
    body: Value,
    user_agent: Option<String>,
) -> Result<IosLocationReport, String> {
    let point = point_from_value(&body)?;
    let accuracy_meters = body
        .get("accuracyMeters")
        .or_else(|| body.get("accuracy_meters"))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0);
    let now = Utc::now().to_rfc3339();
    let report_id = arg_text(&body, "reportId")
        .or_else(|| arg_text(&body, "report_id"))
        .unwrap_or_else(|| format!("IOSLOCREPORT-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let evidence_path = location_report_file_path(&report_id);
    let report = IosLocationReport {
        report_id: report_id.clone(),
        status: "REPORT_RECEIVED".into(),
        lat: point.lat,
        lng: point.lng,
        accuracy_meters,
        source: arg_text(&body, "source").unwrap_or_else(|| "ios_browser_geolocation".into()),
        user_agent,
        reported_at: arg_text(&body, "reportedAt").or_else(|| arg_text(&body, "reported_at")),
        received_at: now,
        evidence_path: evidence_path.display().to_string(),
        note: arg_text(&body, "note"),
    };
    write_location_report(&report)?;
    Ok(report)
}

pub(crate) fn call_ios_location_set_prepare(args: Value) -> Result<Value, String> {
    let point = point_from_args(&args)?;
    let session_id = arg_text(&args, "sessionId")
        .or_else(|| arg_text(&args, "session_id"))
        .unwrap_or_else(|| format!("IOSLOC-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let now = Utc::now().to_rfc3339();
    let evidence_path = evidence_file_path(&session_id);
    let session = IosLocationSession {
        session_id: session_id.clone(),
        status: "WAITING_FOR_OPERATOR_3UTOOLS_APPLY".into(),
        created_at: now.clone(),
        updated_at: now,
        mode: "single_point".into(),
        point: Some(point.clone()),
        target_app: arg_text(&args, "targetApp").or_else(|| arg_text(&args, "target_app")),
        operator_action: "In 3uTools Toolbox > Virtual Location, enter the lat/lng and click Modify virtual location. Keep the iPhone unlocked and trusted.".into(),
        evidence_path: evidence_path.display().to_string(),
        note: arg_text(&args, "note"),
        backend: Some("ios_3utools_gui".into()),
        command_summary: None,
        command_status: None,
        stdout_excerpt: None,
        stderr_excerpt: None,
    };
    write_session_evidence(&session, "prepare")?;
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.clone(), session.clone());
    Ok(json!({
        "status": session.status,
        "sessionId": session_id,
        "driver": "ios_3utools_gui",
        "point": point,
        "threeUTools": detect_3utools_install(),
        "operatorAction": session.operator_action,
        "manualSteps": [
            "Open 3uTools and connect the iPhone via USB.",
            "Unlock the iPhone and tap Trust This Computer if prompted.",
            "Open Toolbox > Virtual Location.",
            "Enter longitude and latitude exactly as returned by this tool.",
            "Click Modify virtual location.",
            "Call ios_location_confirm after the prompt reports success."
        ],
        "evidencePath": session.evidence_path,
        "safety": "Only use on owned/test devices and owned apps; Sirin does not store Apple credentials."
    }))
}

pub(crate) fn call_ios_location_restore_prepare(args: Value) -> Result<Value, String> {
    let session_id = arg_text(&args, "sessionId")
        .or_else(|| arg_text(&args, "session_id"))
        .unwrap_or_else(|| format!("IOSRESTORE-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let now = Utc::now().to_rfc3339();
    let evidence_path = evidence_file_path(&session_id);
    let session = IosLocationSession {
        session_id: session_id.clone(),
        status: "WAITING_FOR_OPERATOR_3UTOOLS_RESTORE".into(),
        created_at: now.clone(),
        updated_at: now,
        mode: "restore_true_location".into(),
        point: None,
        target_app: None,
        operator_action: "In 3uTools Toolbox > Virtual Location, click Restore true location. If the iPhone still reports the virtual location, reboot the iPhone.".into(),
        evidence_path: evidence_path.display().to_string(),
        note: arg_text(&args, "note"),
        backend: Some("ios_3utools_gui".into()),
        command_summary: None,
        command_status: None,
        stdout_excerpt: None,
        stderr_excerpt: None,
    };
    write_session_evidence(&session, "restore_prepare")?;
    sessions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.clone(), session.clone());
    Ok(json!({
        "status": session.status,
        "sessionId": session_id,
        "driver": "ios_3utools_gui",
        "operatorAction": session.operator_action,
        "manualSteps": [
            "Open 3uTools Toolbox > Virtual Location.",
            "Click Restore true location.",
            "If 3uTools asks for a reboot or the device keeps the virtual location, reboot the iPhone.",
            "Call ios_location_confirm with status=RESTORED after validation."
        ],
        "evidencePath": session.evidence_path
    }))
}

pub(crate) fn call_ios_location_confirm(args: Value) -> Result<Value, String> {
    let session_id = arg_text(&args, "sessionId")
        .or_else(|| arg_text(&args, "session_id"))
        .ok_or_else(|| "sessionId is required".to_string())?;
    let status = arg_text(&args, "status").unwrap_or_else(|| "APPLIED".into());
    let mut guard = sessions().lock().unwrap_or_else(|e| e.into_inner());
    let session = guard
        .get_mut(&session_id)
        .ok_or_else(|| format!("iOS location session not found: {session_id}"))?;
    session.status = normalize_confirm_status(&status);
    session.updated_at = Utc::now().to_rfc3339();
    session.note = arg_text(&args, "note").or_else(|| session.note.clone());
    write_session_evidence(session, "confirm")?;
    Ok(json!({
        "status": session.status,
        "session": session,
        "nextAction": if session.mode == "single_point" {
            "Verify the target app sees the expected location, then prepare restore when testing is done."
        } else {
            "Verify the target app/device has returned to true location."
        }
    }))
}

pub(crate) fn call_ios_location_session_status(args: Value) -> Result<Value, String> {
    let session_id = arg_text(&args, "sessionId").or_else(|| arg_text(&args, "session_id"));
    let guard = sessions().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = session_id {
        let session = guard
            .get(&id)
            .ok_or_else(|| format!("iOS location session not found: {id}"))?;
        return Ok(json!({ "session": session }));
    }
    Ok(json!({ "sessions": guard.values().collect::<Vec<_>>() }))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreeUToolsInstall {
    available: bool,
    launcher_path: Option<String>,
    app_path: Option<String>,
    install_dir: Option<PathBuf>,
    source: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CliStatus {
    available: bool,
    command: String,
    path: Option<String>,
}

#[derive(Debug, Clone)]
struct DevLocationBackend {
    name: String,
    command: String,
    style: BackendStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendStyle {
    PyMobileDevice3,
    LibImobileDevice,
}

#[derive(Debug, Clone)]
struct CommandResult {
    success: bool,
    ambiguous: bool,
    stdout: String,
    stderr: String,
}

fn detect_3utools_install() -> ThreeUToolsInstall {
    let env_path = std::env::var("SIRIN_3UTOOLS_PATH").ok().map(PathBuf::from);
    let candidates = [
        env_path,
        Some(PathBuf::from(DEFAULT_3UTOOLS_LAUNCHER)),
        Some(PathBuf::from(DEFAULT_3UTOOLS_EXE)),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.exists() {
            let install_dir = candidate.parent().map(PathBuf::from);
            let launcher_path = if candidate
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("3uLauncher.exe"))
                .unwrap_or(false)
            {
                Some(candidate.display().to_string())
            } else {
                install_dir
                    .as_ref()
                    .map(|dir| dir.join("3uLauncher.exe"))
                    .filter(|p| p.exists())
                    .map(|p| p.display().to_string())
            };
            let app_path = install_dir
                .as_ref()
                .map(|dir| dir.join("3uTools.exe"))
                .filter(|p| p.exists())
                .map(|p| p.display().to_string());
            return ThreeUToolsInstall {
                available: true,
                launcher_path,
                app_path,
                install_dir,
                source: if std::env::var("SIRIN_3UTOOLS_PATH").is_ok() {
                    "SIRIN_3UTOOLS_PATH".into()
                } else {
                    "default_windows_path".into()
                },
            };
        }
    }
    ThreeUToolsInstall {
        available: false,
        launcher_path: None,
        app_path: None,
        install_dir: Some(PathBuf::from(DEFAULT_3UTOOLS_DIR)),
        source: "not_found".into(),
    }
}

fn cli_status(command: &str) -> CliStatus {
    let path = find_command(command);
    CliStatus {
        available: path.is_some(),
        command: command.into(),
        path,
    }
}

fn find_command(command: &str) -> Option<String> {
    let lookup = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(lookup)
        .no_window()
        .arg(command)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn select_backend_name(requested: &str) -> String {
    select_backend(requested)
        .map(|backend| backend.name)
        .unwrap_or_else(|_| "none".into())
}

fn select_backend(requested: &str) -> Result<DevLocationBackend, String> {
    let normalized = requested.trim().to_ascii_lowercase();
    let allow_pymobile =
        normalized == "auto" || normalized == "pymobiledevice3" || normalized == "pymobile";
    let allow_libimobile = normalized == "auto"
        || normalized == "libimobiledevice"
        || normalized == "idevicesetlocation";
    if allow_pymobile {
        if let Some(command) = find_command("pymobiledevice3") {
            return Ok(DevLocationBackend {
                name: "pymobiledevice3".into(),
                command,
                style: BackendStyle::PyMobileDevice3,
            });
        }
    }
    if allow_libimobile {
        if let Some(command) = find_command("idevicesetlocation") {
            return Ok(DevLocationBackend {
                name: "libimobiledevice".into(),
                command,
                style: BackendStyle::LibImobileDevice,
            });
        }
    }
    Err(format!(
        "no iOS developer location backend available for '{requested}'; install pymobiledevice3 or libimobiledevice idevicesetlocation"
    ))
}

impl DevLocationBackend {
    fn set_summary(&self, point: &IosPoint) -> String {
        match self.style {
            BackendStyle::PyMobileDevice3 => format!(
                "pymobiledevice3 developer dvt simulate-location set --userspace -- {:.7} {:.7}",
                point.lat, point.lng
            ),
            BackendStyle::LibImobileDevice => {
                format!("idevicesetlocation -- {:.7} {:.7}", point.lat, point.lng)
            }
        }
    }

    fn reset_summary(&self) -> String {
        match self.style {
            BackendStyle::PyMobileDevice3 => {
                "pymobiledevice3 developer dvt simulate-location clear --userspace".into()
            }
            BackendStyle::LibImobileDevice => "idevicesetlocation reset".into(),
        }
    }

    fn run_set(&self, point: &IosPoint) -> Result<CommandResult, String> {
        match self.style {
            BackendStyle::PyMobileDevice3 => run_command(
                &self.command,
                &[
                    "developer",
                    "dvt",
                    "simulate-location",
                    "set",
                    "--userspace",
                    "--",
                    &format!("{:.7}", point.lat),
                    &format!("{:.7}", point.lng),
                ],
            ),
            BackendStyle::LibImobileDevice => run_command(
                &self.command,
                &[
                    "--",
                    &format!("{:.7}", point.lat),
                    &format!("{:.7}", point.lng),
                ],
            ),
        }
    }

    fn run_reset(&self) -> Result<CommandResult, String> {
        match self.style {
            BackendStyle::PyMobileDevice3 => run_command(
                &self.command,
                &[
                    "developer",
                    "dvt",
                    "simulate-location",
                    "clear",
                    "--userspace",
                ],
            ),
            BackendStyle::LibImobileDevice => run_command(&self.command, &["reset"]),
        }
    }
}

fn run_command(command: &str, args: &[&str]) -> Result<CommandResult, String> {
    let output = Command::new(command)
        .no_window()
        .args(args)
        .output()
        .map_err(|e| format!("failed to run {command}: {e}"))?;
    let stdout = redact_sensitive_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = redact_sensitive_output(&String::from_utf8_lossy(&output.stderr));
    let failed = looks_like_cli_failure(&stdout, &stderr);
    Ok(CommandResult {
        success: output.status.success() && !failed,
        ambiguous: output.status.success() && !failed && looks_like_cli_ambiguous(&stdout, &stderr),
        stdout,
        stderr,
    })
}

fn looks_like_cli_failure(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    [
        " error ",
        "error:",
        "device is not connected",
        "no device",
        "not connected",
        "connection refused",
        "failed",
        "traceback",
    ]
    .iter()
    .any(|needle| combined.contains(needle))
}

fn looks_like_cli_ambiguous(stdout: &str, stderr: &str) -> bool {
    let combined = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    ["aborted.", "press enter to exit"]
        .iter()
        .any(|needle| combined.contains(needle))
}

fn new_session(
    session_id: &str,
    mode: &str,
    point: Option<IosPoint>,
    operator_action: &str,
    target_app: Option<String>,
    note: Option<String>,
) -> IosLocationSession {
    let now = Utc::now().to_rfc3339();
    IosLocationSession {
        session_id: session_id.into(),
        status: "RUNNING".into(),
        created_at: now.clone(),
        updated_at: now,
        mode: mode.into(),
        point,
        target_app,
        operator_action: operator_action.into(),
        evidence_path: evidence_file_path(session_id).display().to_string(),
        note,
        backend: None,
        command_summary: None,
        command_status: None,
        stdout_excerpt: None,
        stderr_excerpt: None,
    }
}

fn apply_command_result(session: &mut IosLocationSession, result: CommandResult) {
    session.updated_at = Utc::now().to_rfc3339();
    session.command_status = Some(
        if result.ambiguous {
            "ambiguous"
        } else if result.success {
            "success"
        } else {
            "failed"
        }
        .into(),
    );
    session.status = if result.ambiguous {
        "NEEDS_OPERATOR_VERIFICATION".into()
    } else if result.success {
        if session.mode == "developer_location_reset" {
            "RESTORED".into()
        } else {
            "APPLIED".into()
        }
    } else {
        "FAILED".into()
    };
    session.stdout_excerpt = excerpt(&result.stdout, 1600);
    session.stderr_excerpt = excerpt(&result.stderr, 1600);
}

fn redacted_connected_device_summary() -> Value {
    let mut sources = Vec::new();
    let mut available = false;
    if let Some(command) = find_command("pymobiledevice3") {
        if let Ok(result) = run_command(&command, &["usbmux", "list"]) {
            let count = pymobiledevice3_device_count(&result.stdout);
            available |= result.success && count > 0;
            sources.push(json!({
                "source": "pymobiledevice3 usbmux list",
                "success": result.success,
                "deviceCount": count,
                "stdoutExcerpt": excerpt(&result.stdout, 1200),
                "stderrExcerpt": excerpt(&result.stderr, 1200)
            }));
        }
    }
    if let Some(command) = find_command("idevice_id") {
        if let Ok(result) = run_command(&command, &["-l"]) {
            let count = result
                .stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            available |= result.success && count > 0;
            sources.push(json!({
                "source": "idevice_id -l",
                "success": result.success,
                "deviceCount": count,
                "identifiersRedacted": true,
                "stderrExcerpt": excerpt(&result.stderr, 1200)
            }));
        }
    }
    json!({
        "available": available,
        "sources": sources,
        "identifiersRedacted": true
    })
}

fn pymobiledevice3_device_count(stdout: &str) -> usize {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return 0;
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return value.as_array().map(Vec::len).unwrap_or(0);
    }
    trimmed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

fn install_hints() -> Value {
    json!({
        "pymobiledevice3": "Install a Windows-compatible pymobiledevice3 CLI, then ensure `pymobiledevice3` is on PATH.",
        "libimobiledevice": "Install libimobiledevice tools that include idevicesetlocation and idevice_id, then ensure both are on PATH.",
        "deviceRequirements": [
            "Use an owned/test iPhone.",
            "Connect via USB, unlock, and trust this computer.",
            "Developer Mode/developer image service may be required by iOS version.",
            "Reset or reboot after tests if simulated location remains active."
        ]
    })
}

fn device_cache_summary(install_dir: Option<&PathBuf>) -> Value {
    let Some(dir) = install_dir else {
        return json!({"available": false});
    };
    let device_info = dir.join("cache").join("files").join("deviceinfo.txt");
    let devices_table = dir
        .join("cache")
        .join("devices_table")
        .join("devices_table.txt");
    json!({
        "deviceInfoPath": device_info.display().to_string(),
        "deviceInfoExists": device_info.exists(),
        "devicesTablePath": devices_table.display().to_string(),
        "devicesTableExists": devices_table.exists(),
        "deviceInfoSummary": device_info_summary(&device_info),
    })
}

fn apple_mobile_device_service_status() -> Value {
    if !cfg!(windows) {
        return json!({"status": "unsupported_non_windows"});
    }
    let output = Command::new("sc")
        .no_window()
        .args(["query", "Apple Mobile Device Service"])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            json!({
                "status": if text.contains("RUNNING") { "RUNNING" } else { "INSTALLED_NOT_RUNNING_OR_UNKNOWN" },
                "raw": text.lines().take(8).collect::<Vec<_>>().join("\n")
            })
        }
        Ok(output) => json!({
            "status": "NOT_FOUND_OR_QUERY_FAILED",
            "stderr": String::from_utf8_lossy(&output.stderr).trim()
        }),
        Err(e) => json!({"status": "QUERY_FAILED", "error": e.to_string()}),
    }
}

fn process_running(image_name: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let output = Command::new("tasklist")
        .no_window()
        .args(["/FI", &format!("IMAGENAME eq {image_name}")])
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains(&image_name.to_ascii_lowercase()),
        _ => false,
    }
}

fn process_running_pid(pid: u64) -> bool {
    if !cfg!(windows) || pid == 0 {
        return false;
    }
    let output = Command::new("tasklist")
        .no_window()
        .args(["/FI", &format!("PID eq {pid}")])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let pid_text = pid.to_string();
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.split_whitespace().any(|part| part == pid_text))
        }
        _ => false,
    }
}

fn stop_process_pid(pid: u64) -> Result<CommandResult, String> {
    if pid == 0 {
        return Err("pid must be greater than zero".into());
    }
    let pid_text = pid.to_string();
    if cfg!(windows) {
        run_command("taskkill", &["/PID", pid_text.as_str(), "/F"])
    } else {
        run_command("kill", &["-TERM", pid_text.as_str()])
    }
}

fn point_from_args(args: &Value) -> Result<IosPoint, String> {
    if let Some(point) = args.get("point") {
        return point_from_value(point);
    }
    point_from_value(args)
}

#[derive(Debug)]
struct IosRouteInput {
    mode: String,
    city: Option<String>,
    center: Option<IosPoint>,
    radius_m: Option<f64>,
    route_source: String,
    road_provider: Option<String>,
    road_routed: bool,
    seed_waypoints: Vec<IosPoint>,
    road_route_points: Vec<IosPoint>,
    points: Vec<IosPoint>,
    route_quality: Option<String>,
}

struct IosRoamRouteChoice {
    seed_waypoints: Vec<IosPoint>,
    points: Vec<IosPoint>,
    road_routed: bool,
    radius_m: f64,
    detour_ratio: Option<f64>,
    route_loop_metrics: RouteLoopMetrics,
    route_quality: String,
}

fn ios_route_input_from_args(args: &Value) -> Result<IosRouteInput, String> {
    if has_route_points(args) {
        let points = route_points_from_args(args)?;
        return Ok(IosRouteInput {
            mode: "manual_road_polyline".into(),
            city: None,
            center: None,
            radius_m: None,
            route_source: "provided_road_polyline".into(),
            road_provider: None,
            road_routed: false,
            seed_waypoints: Vec::new(),
            road_route_points: points.clone(),
            points,
            route_quality: None,
        });
    }
    ios_roam_route_from_args(args)
}

fn ios_roam_route_from_args(args: &Value) -> Result<IosRouteInput, String> {
    let center = if let Some(center) = args.get("center") {
        point_from_value(center)?
    } else if let Some(point) = args
        .get("currentLocation")
        .or_else(|| args.get("current_location"))
    {
        point_from_value(point)?
    } else if has_point_fields(args) {
        point_from_value(args)?
    } else if let Some(city) = arg_text(args, "city") {
        city_center(&city)
            .ok_or_else(|| format!("unknown city: {city}; pass center/currentLocation instead"))?
    } else {
        return Err(
            "current-city roam requires center/currentLocation/lat,lng or a supported city; refusing to fallback to Taipei".into()
        );
    };
    let city = arg_text(args, "city").unwrap_or_else(|| "current_city".into());
    let radius_m = f64_param(args, "radiusMeters", DEFAULT_ROAM_RADIUS_M)
        .or_else(|| f64_param(args, "radius_meters", DEFAULT_ROAM_RADIUS_M))
        .unwrap_or(DEFAULT_ROAM_RADIUS_M)
        .clamp(100.0, 10_000.0);
    let waypoint_count = u64_param(args, "waypoints", DEFAULT_ROAM_WAYPOINTS)
        .unwrap_or(DEFAULT_ROAM_WAYPOINTS)
        .clamp(4, 32) as usize;
    let road_provider = arg_text(args, "roadProvider")
        .or_else(|| arg_text(args, "road_provider"))
        .or_else(|| std::env::var("SIRIN_ROAD_PROVIDER").ok())
        .unwrap_or_else(|| DEFAULT_ROAD_PROVIDER.into())
        .to_ascii_lowercase();
    let allow_synthetic_fallback = bool_param(args, "allowSyntheticFallback", false)
        || bool_param(args, "allow_synthetic_fallback", false);
    let requested_distance_m = requested_distance_from_args(args);
    let route_choice = if road_provider == "synthetic" || road_provider == "none" {
        let seed_waypoints = roam_seed_waypoints(&center, radius_m, waypoint_count);
        IosRoamRouteChoice {
            seed_waypoints: seed_waypoints.clone(),
            points: seed_waypoints,
            road_routed: false,
            radius_m,
            detour_ratio: None,
            route_loop_metrics: RouteLoopMetrics::default(),
            route_quality: "SYNTHETIC_ROUTE".into(),
        }
    } else {
        match best_road_roam_route(
            &center,
            radius_m,
            waypoint_count,
            requested_distance_m,
            args,
        ) {
            Ok(choice) => choice,
            Err(e) if allow_synthetic_fallback => {
                tracing::warn!(
                    "[ios_location] road routing failed, falling back to synthetic route: {e}"
                );
                let seed_waypoints = roam_seed_waypoints(&center, radius_m, waypoint_count);
                IosRoamRouteChoice {
                    seed_waypoints: seed_waypoints.clone(),
                    points: seed_waypoints,
                    road_routed: false,
                    radius_m,
                    detour_ratio: None,
                    route_loop_metrics: RouteLoopMetrics::default(),
                    route_quality: "SYNTHETIC_FALLBACK".into(),
                }
            }
            Err(e) => return Err(e),
        }
    };
    Ok(IosRouteInput {
        mode: "current_city_roam_road".into(),
        city: Some(city),
        center: Some(center),
        radius_m: Some(route_choice.radius_m),
        route_source: "current_city_osrm_roam".into(),
        road_provider: Some(road_provider),
        road_routed: route_choice.road_routed,
        seed_waypoints: route_choice.seed_waypoints,
        road_route_points: route_choice.points.clone(),
        points: route_choice.points,
        route_quality: Some(route_choice.route_quality),
    })
}

fn requested_distance_from_args(args: &Value) -> f64 {
    let speed_kmh = f64_param(args, "speedKmh", DEFAULT_ROUTE_SPEED_KMH)
        .or_else(|| f64_param(args, "speed_kmh", DEFAULT_ROUTE_SPEED_KMH))
        .unwrap_or(DEFAULT_ROUTE_SPEED_KMH)
        .clamp(1.0, 130.0);
    let duration_seconds = u64_param(args, "durationSeconds", DEFAULT_ROUTE_DURATION_SECONDS)
        .or_else(|| u64_param(args, "duration_seconds", DEFAULT_ROUTE_DURATION_SECONDS))
        .unwrap_or(DEFAULT_ROUTE_DURATION_SECONDS)
        .clamp(1, 86_400);
    speed_kmh * 1000.0 / 3600.0 * duration_seconds as f64
}

fn movement_profile_from_args(args: &Value) -> String {
    let value = arg_text(args, "movementProfile")
        .or_else(|| arg_text(args, "movement_profile"))
        .or_else(|| arg_text(args, "profile"))
        .unwrap_or_else(|| "roam".into());
    match value.trim().to_ascii_lowercase().as_str() {
        "bicycle" | "bike" | "cycling" | "cycle" => "bicycle".into(),
        _ => "roam".into(),
    }
}

fn roam_seed_waypoints(center: &IosPoint, radius_m: f64, waypoint_count: usize) -> Vec<IosPoint> {
    let mut seed_waypoints = Vec::with_capacity(waypoint_count + 1);
    seed_waypoints.push(center.clone());
    let heading = 31.0_f64.to_radians();
    let turn_step = 42.0_f64.to_radians();
    for idx in 0..waypoint_count {
        let progress = if waypoint_count <= 1 {
            1.0
        } else {
            idx as f64 / (waypoint_count - 1) as f64
        };
        let angle = heading + idx as f64 * turn_step;
        let scale = (0.22 + progress * 0.78).min(1.0);
        seed_waypoints.push(offset_point(center, radius_m * scale, angle));
    }
    seed_waypoints
}

fn best_road_roam_route(
    center: &IosPoint,
    radius_m: f64,
    waypoint_count: usize,
    requested_distance_m: f64,
    args: &Value,
) -> Result<IosRoamRouteChoice, String> {
    let requested_radius_m = (requested_distance_m * 0.9).clamp(radius_m, 10_000.0);
    let expanded_waypoints = waypoint_count.saturating_add(8).min(32).max(8);
    let mut attempts = vec![
        (radius_m, waypoint_count),
        (requested_radius_m, 1),
        ((radius_m * 1.5).min(10_000.0), expanded_waypoints),
        (requested_radius_m, expanded_waypoints),
        (requested_radius_m, waypoint_count.saturating_sub(2).max(4)),
        (
            (radius_m * 0.5).max(100.0),
            waypoint_count.saturating_sub(2).max(4),
        ),
        (
            (radius_m * 0.25).max(100.0),
            waypoint_count.saturating_sub(4).max(4),
        ),
        (100.0, 2),
    ];
    attempts.dedup_by(|a, b| (a.0 - b.0).abs() < 0.001 && a.1 == b.1);

    let mut best: Option<IosRoamRouteChoice> = None;
    let mut last_err: Option<String> = None;
    for (attempt_radius_m, attempt_waypoints) in attempts {
        let seed_waypoints = roam_seed_waypoints(center, attempt_radius_m, attempt_waypoints);
        match road_route_points(&seed_waypoints, args) {
            Ok(points) => {
                let distance_m = route_distance_m(&points);
                let detour_ratio = if requested_distance_m > f64::EPSILON {
                    Some(distance_m / requested_distance_m)
                } else {
                    None
                };
                let snap_offset_m = points.first().map(|first| haversine_m(center, first));
                let route_loop_metrics = route_loop_metrics(&points);
                let route_quality =
                    route_quality_label(true, detour_ratio, snap_offset_m, &route_loop_metrics);
                let choice = IosRoamRouteChoice {
                    seed_waypoints,
                    points,
                    road_routed: true,
                    radius_m: attempt_radius_m,
                    detour_ratio,
                    route_loop_metrics,
                    route_quality: route_quality.into(),
                };
                let distance_sufficient =
                    route_distance_sufficient(distance_m, requested_distance_m);
                let acceptable = distance_sufficient
                    && detour_ratio
                        .map(|ratio| ratio <= MAX_ACCEPTABLE_DETOUR_RATIO)
                        .unwrap_or(true)
                    && route_loop_acceptable(&choice.route_loop_metrics);
                best = match best {
                    Some(existing)
                        if route_choice_score(&existing) <= route_choice_score(&choice) =>
                    {
                        Some(existing)
                    }
                    _ => Some(choice),
                };
                if acceptable {
                    break;
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    best.ok_or_else(|| last_err.unwrap_or_else(|| "road router produced no route".into()))
}

fn route_choice_score(choice: &IosRoamRouteChoice) -> f64 {
    let loop_penalty = if route_loop_acceptable(&choice.route_loop_metrics) {
        0.0
    } else {
        1000.0
            + choice.route_loop_metrics.backtrack_count as f64 * 20.0
            + choice.route_loop_metrics.repeated_segment_count as f64 * 10.0
            + choice.route_loop_metrics.overlap_ratio * 100.0
    };
    let detour_ratio = choice.detour_ratio.unwrap_or(f64::MAX);
    let distance_penalty = if detour_ratio < 1.0 {
        500.0 + (1.0 - detour_ratio) * 100.0
    } else if detour_ratio > MAX_ACCEPTABLE_DETOUR_RATIO {
        100.0 + (detour_ratio - MAX_ACCEPTABLE_DETOUR_RATIO) * 10.0
    } else {
        (detour_ratio - 1.0).abs()
    };
    loop_penalty + distance_penalty
}

fn route_distance_sufficient(route_distance_m: f64, requested_distance_m: f64) -> bool {
    route_distance_m + 0.001 >= requested_distance_m
}

fn route_quality_label(
    road_routed: bool,
    detour_ratio: Option<f64>,
    snap_offset_m: Option<f64>,
    route_loop_metrics: &RouteLoopMetrics,
) -> &'static str {
    if !road_routed {
        return "SYNTHETIC";
    }
    if snap_offset_m.unwrap_or(0.0) > 250.0 {
        return "ROAD_START_FAR_FROM_ORIGIN";
    }
    if !route_loop_acceptable(route_loop_metrics) {
        return "ROAD_REPEATS_OR_BACKTRACKS";
    }
    match detour_ratio {
        Some(ratio) if ratio > MAX_ACCEPTABLE_DETOUR_RATIO => "ROAD_DETOUR_HIGH",
        Some(ratio) if ratio > 1.8 => "ROAD_DETOUR_MODERATE",
        Some(_) => "ROAD_ROUTE_GOOD",
        None => "ROAD_ROUTE_UNKNOWN",
    }
}

struct RoutePlanGate {
    status: &'static str,
    playable: bool,
    block_reason: Option<&'static str>,
}

fn route_plan_gate(
    route_distance_m: f64,
    requested_distance_m: f64,
    route_quality: &str,
    route_loop_metrics: &RouteLoopMetrics,
) -> RoutePlanGate {
    if !route_distance_sufficient(route_distance_m, requested_distance_m) {
        return RoutePlanGate {
            status: "INSUFFICIENT_ROAD_DISTANCE_REQUIRES_LONGER_POLYLINE",
            playable: false,
            block_reason: Some("route distance is shorter than requested playback distance"),
        };
    }
    if !route_loop_acceptable(route_loop_metrics) {
        return RoutePlanGate {
            status: "ROUTE_REJECTED_REGENERATE_REQUIRED",
            playable: false,
            block_reason: Some("route repeats road segments or backtracks"),
        };
    }
    match route_quality {
        "ROAD_START_FAR_FROM_ORIGIN" => RoutePlanGate {
            status: "ROUTE_REJECTED_REGENERATE_REQUIRED",
            playable: false,
            block_reason: Some("road route starts too far from requested origin"),
        },
        "ROAD_DETOUR_HIGH" => RoutePlanGate {
            status: "ROUTE_REJECTED_REGENERATE_REQUIRED",
            playable: false,
            block_reason: Some("road route detour ratio is too high"),
        },
        _ => RoutePlanGate {
            status: "READY_REQUIRES_REAL_IPHONE",
            playable: true,
            block_reason: None,
        },
    }
}

fn route_plan_next_action(plan: &IosRoutePlan) -> &'static str {
    if plan.route_playable {
        return "Connect and trust an owned/test iPhone, ensure pymobiledevice3 or idevicesetlocation is available, then execute ticks with ios_dev_location_set/reset support.";
    }
    match plan.route_quality.as_str() {
        "ROAD_REPEATS_OR_BACKTRACKS" => {
            "Regenerate the route with a different start point, smaller radius, or fewer waypoints before playback."
        }
        "ROAD_START_FAR_FROM_ORIGIN" => {
            "Choose a start point closer to mapped roads, then regenerate before playback."
        }
        "ROAD_DETOUR_HIGH" => {
            "Reduce radius or duration, then regenerate a road route before playback."
        }
        _ => "Regenerate a playable route before starting iOS location playback.",
    }
}

fn route_loop_acceptable(metrics: &RouteLoopMetrics) -> bool {
    metrics.backtrack_count <= MAX_ROUTE_BACKTRACK_COUNT
        && metrics.repeated_segment_count <= MAX_ROUTE_REPEATED_SEGMENT_COUNT
}

fn route_loop_metrics(points: &[IosPoint]) -> RouteLoopMetrics {
    if points.len() < 2 {
        return RouteLoopMetrics::default();
    }

    let mut seen_segments = HashSet::new();
    let mut repeated_segment_count = 0;
    let mut eligible_segment_count = 0;
    for pair in points.windows(2) {
        if haversine_m(&pair[0], &pair[1]) < ROUTE_LOOP_SEGMENT_MIN_M {
            continue;
        }
        eligible_segment_count += 1;
        let key = undirected_segment_key(&pair[0], &pair[1]);
        if !seen_segments.insert(key) {
            repeated_segment_count += 1;
        }
    }

    let mut backtrack_count = 0;
    for triple in points.windows(3) {
        if haversine_m(&triple[0], &triple[1]) < ROUTE_LOOP_SEGMENT_MIN_M
            || haversine_m(&triple[1], &triple[2]) < ROUTE_LOOP_SEGMENT_MIN_M
        {
            continue;
        }
        if route_turn_angle_deg(&triple[0], &triple[1], &triple[2]) >= ROUTE_BACKTRACK_ANGLE_DEG {
            backtrack_count += 1;
        }
    }

    RouteLoopMetrics {
        backtrack_count,
        repeated_segment_count,
        overlap_ratio: if eligible_segment_count == 0 {
            0.0
        } else {
            repeated_segment_count as f64 / eligible_segment_count as f64
        },
    }
}

fn undirected_segment_key(a: &IosPoint, b: &IosPoint) -> ((i64, i64), (i64, i64)) {
    let a_key = quantized_point_key(a);
    let b_key = quantized_point_key(b);
    if a_key <= b_key {
        (a_key, b_key)
    } else {
        (b_key, a_key)
    }
}

fn quantized_point_key(point: &IosPoint) -> (i64, i64) {
    (
        (point.lat * 10_000.0).round() as i64,
        (point.lng * 10_000.0).round() as i64,
    )
}

fn route_turn_angle_deg(a: &IosPoint, b: &IosPoint, c: &IosPoint) -> f64 {
    let incoming = route_segment_vector_m(a, b);
    let outgoing = route_segment_vector_m(b, c);
    let incoming_len = (incoming.0 * incoming.0 + incoming.1 * incoming.1).sqrt();
    let outgoing_len = (outgoing.0 * outgoing.0 + outgoing.1 * outgoing.1).sqrt();
    if incoming_len <= f64::EPSILON || outgoing_len <= f64::EPSILON {
        return 0.0;
    }
    let dot = incoming.0 * outgoing.0 + incoming.1 * outgoing.1;
    (dot / (incoming_len * outgoing_len))
        .clamp(-1.0, 1.0)
        .acos()
        .to_degrees()
}

fn route_segment_vector_m(a: &IosPoint, b: &IosPoint) -> (f64, f64) {
    let mean_lat_rad = ((a.lat + b.lat) * 0.5).to_radians();
    let lat_m = (b.lat - a.lat) * 111_320.0;
    let lng_m = (b.lng - a.lng) * 111_320.0 * mean_lat_rad.cos().abs().max(0.01);
    (lng_m, lat_m)
}

fn has_route_points(args: &Value) -> bool {
    args.get("points").is_some()
        || args.get("roadPolyline").is_some()
        || args.get("road_polyline").is_some()
        || args
            .get("route")
            .and_then(|route| route.get("points"))
            .is_some()
}

fn has_point_fields(args: &Value) -> bool {
    args.get("lat").is_some()
        || args.get("latitude").is_some()
        || args.get("lng").is_some()
        || args.get("lon").is_some()
        || args.get("longitude").is_some()
}

fn route_points_from_args(args: &Value) -> Result<Vec<IosPoint>, String> {
    let points_value = args
        .get("points")
        .or_else(|| args.get("roadPolyline"))
        .or_else(|| args.get("road_polyline"))
        .or_else(|| args.get("route").and_then(|route| route.get("points")))
        .ok_or_else(|| {
            "road-following route requires points/roadPolyline/route.points with at least two road polyline points".to_string()
        })?;
    let values = points_value
        .as_array()
        .ok_or_else(|| "route points must be an array".to_string())?;
    if values.len() < 2 {
        return Err("route requires at least two road polyline points".into());
    }
    values.iter().map(point_from_value).collect()
}

fn point_from_value(value: &Value) -> Result<IosPoint, String> {
    let lat = value
        .get("lat")
        .or_else(|| value.get("latitude"))
        .and_then(Value::as_f64)
        .ok_or("lat is required")?;
    let lng = value
        .get("lng")
        .or_else(|| value.get("lon"))
        .or_else(|| value.get("longitude"))
        .and_then(Value::as_f64)
        .ok_or("lng is required")?;
    if !(-90.0..=90.0).contains(&lat) {
        return Err("lat must be between -90 and 90".into());
    }
    if !(-180.0..=180.0).contains(&lng) {
        return Err("lng must be between -180 and 180".into());
    }
    Ok(IosPoint { lat, lng })
}

fn road_route_points(seed_waypoints: &[IosPoint], args: &Value) -> Result<Vec<IosPoint>, String> {
    if seed_waypoints.len() < 2 {
        return Err("road route requires at least two seed waypoints".into());
    }
    let base_url = arg_text(args, "roadRouterUrl")
        .or_else(|| arg_text(args, "road_router_url"))
        .or_else(|| std::env::var("SIRIN_OSRM_BASE_URL").ok())
        .unwrap_or_else(|| DEFAULT_OSRM_BASE_URL.into());
    let profile = arg_text(args, "roadProfile")
        .or_else(|| arg_text(args, "road_profile"))
        .unwrap_or_else(|| "driving".into());
    if !profile
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("roadProfile contains unsupported characters".into());
    }
    let coords = seed_waypoints
        .iter()
        .map(|p| format!("{:.7},{:.7}", p.lng, p.lat))
        .collect::<Vec<_>>()
        .join(";");
    let url = format!(
        "{}/route/v1/{}/{}?overview=full&geometries=geojson&steps=false",
        base_url.trim_end_matches('/'),
        profile,
        coords
    );
    let timeout_seconds = u64_param(args, "roadTimeoutSeconds", 15)
        .or_else(|| u64_param(args, "road_timeout_seconds", 15))
        .unwrap_or(15)
        .clamp(1, 120);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .build()
        .map_err(|e| format!("road router client: {e}"))?;
    let resp = client
        .get(&url)
        .header("User-Agent", "Sirin ios-location road-router")
        .send()
        .map_err(|e| format!("road router request failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .map_err(|e| format!("road router response parse failed: {e}"))?;
    if !status.is_success() {
        return Err(format!("road router HTTP {status}: {body}"));
    }
    if body.get("code").and_then(Value::as_str) != Some("Ok") {
        return Err(format!(
            "road router returned code {:?}: {}",
            body.get("code"),
            body.get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
        ));
    }
    let coords = body
        .pointer("/routes/0/geometry/coordinates")
        .and_then(Value::as_array)
        .ok_or_else(|| "road router response missing routes[0].geometry.coordinates".to_string())?;
    let points = coords
        .iter()
        .map(|coord| {
            let arr = coord
                .as_array()
                .ok_or_else(|| "road route coordinate must be an array".to_string())?;
            let lng = arr
                .first()
                .and_then(Value::as_f64)
                .ok_or_else(|| "road route coordinate lng missing".to_string())?;
            let lat = arr
                .get(1)
                .and_then(Value::as_f64)
                .ok_or_else(|| "road route coordinate lat missing".to_string())?;
            Ok(IosPoint { lat, lng })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if points.len() < 2 {
        return Err("road router returned fewer than two route points".into());
    }
    Ok(points)
}

fn city_center(city: &str) -> Option<IosPoint> {
    match city.trim().to_ascii_lowercase().as_str() {
        "taipei" | "台北" | "臺北" => Some(IosPoint {
            lat: 25.033964,
            lng: 121.564468,
        }),
        "new_taipei" | "new taipei" | "新北" => Some(IosPoint {
            lat: 25.011002,
            lng: 121.465661,
        }),
        "taichung" | "台中" | "臺中" => Some(IosPoint {
            lat: 24.147736,
            lng: 120.673648,
        }),
        "kaohsiung" | "高雄" => Some(IosPoint {
            lat: 22.627278,
            lng: 120.301435,
        }),
        "chiang_mai" | "chiang mai" | "清邁" | "清迈" => Some(IosPoint {
            lat: 18.788343,
            lng: 98.985300,
        }),
        _ => None,
    }
}

fn offset_point(center: &IosPoint, distance_m: f64, bearing_rad: f64) -> IosPoint {
    let angular = distance_m / EARTH_RADIUS_M;
    let lat1 = center.lat.to_radians();
    let lng1 = center.lng.to_radians();
    let lat2 = (lat1.sin() * angular.cos() + lat1.cos() * angular.sin() * bearing_rad.cos()).asin();
    let lng2 = lng1
        + (bearing_rad.sin() * angular.sin() * lat1.cos())
            .atan2(angular.cos() - lat1.sin() * lat2.sin());
    IosPoint {
        lat: lat2.to_degrees(),
        lng: lng2.to_degrees(),
    }
}

fn f64_param(args: &Value, key: &str, default: f64) -> Option<f64> {
    args.get(key)
        .and_then(Value::as_f64)
        .map(|value| if value.is_finite() { value } else { default })
}

fn u64_param(args: &Value, key: &str, default: u64) -> Option<u64> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|value| if value == 0 { default } else { value })
}

fn bool_param(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn arg_text(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn normalize_confirm_status(status: &str) -> String {
    match status.trim().to_ascii_uppercase().as_str() {
        "APPLIED" | "SUCCESS" | "DONE" => "APPLIED".into(),
        "RESTORED" | "RESTORE_DONE" => "RESTORED".into(),
        "FAILED" | "ERROR" => "FAILED".into(),
        "CANCELLED" | "CANCELED" => "CANCELLED".into(),
        _ => "OPERATOR_CONFIRMED".into(),
    }
}

fn evidence_file_path(session_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join(format!("{}.json", sanitize_file_part(session_id)))
}

fn route_evidence_file_path(route_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join("routes")
        .join(format!("{}.json", sanitize_file_part(route_id)))
}

fn route_gpx_file_path(route_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join("routes")
        .join(format!("{}.gpx", sanitize_file_part(route_id)))
}

fn route_run_file_path(run_id: &str, file_name: &str) -> PathBuf {
    route_run_dir_path(run_id).join(file_name)
}

fn route_run_dir_path(run_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join("route_runs")
        .join(sanitize_file_part(run_id))
}

fn route_runs_dir_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join("route_runs")
}

fn latest_location_report_path() -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join("current")
        .join("reported_location.json")
}

fn location_report_file_path(report_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join("reports")
        .join(format!("{}.json", sanitize_file_part(report_id)))
}

fn current_location_evidence_file_path(request_id: &str) -> PathBuf {
    crate::platform::app_data_dir()
        .join("ios_location")
        .join("current")
        .join(format!("{}.json", sanitize_file_part(request_id)))
}

fn write_location_report(report: &IosLocationReport) -> Result<(), String> {
    let latest_path = latest_location_report_path();
    let evidence_path = PathBuf::from(&report.evidence_path);
    if let Some(parent) = latest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create current-location report dir {}: {e}",
                parent.display()
            )
        })?;
    }
    if let Some(parent) = evidence_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create location report evidence dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let body = serde_json::to_string_pretty(report)
        .map_err(|e| format!("failed to serialize location report: {e}"))?;
    std::fs::write(&latest_path, &body).map_err(|e| {
        format!(
            "failed to write latest location report {}: {e}",
            latest_path.display()
        )
    })?;
    std::fs::write(&evidence_path, body).map_err(|e| {
        format!(
            "failed to write location report evidence {}: {e}",
            evidence_path.display()
        )
    })
}

fn write_session_evidence(session: &IosLocationSession, event: &str) -> Result<(), String> {
    let path = PathBuf::from(&session.evidence_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create evidence dir {}: {e}", parent.display()))?;
    }
    let body = json!({
        "event": event,
        "recordedAt": Utc::now().to_rfc3339(),
        "session": session,
        "driver": if session.mode.starts_with("developer_location") {
            "ios_developer_location"
        } else {
            "ios_3utools_gui"
        },
        "appleMobileDeviceService": apple_mobile_device_service_status(),
    });
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&body)
            .map_err(|e| format!("failed to serialize evidence: {e}"))?,
    )
    .map_err(|e| format!("failed to write evidence {}: {e}", path.display()))
}

fn write_current_location_evidence(path: &PathBuf, response: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create current-location evidence dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let body = json!({
        "event": "current_location_probe",
        "recordedAt": Utc::now().to_rfc3339(),
        "driver": "ios_developer_location",
        "response": response,
        "backendReadiness": call_ios_dev_location_status(json!({}))?,
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&body)
            .map_err(|e| format!("failed to serialize current-location evidence: {e}"))?,
    )
    .map_err(|e| {
        format!(
            "failed to write current-location evidence {}: {e}",
            path.display()
        )
    })
}

fn write_route_evidence(plan: &IosRoutePlan, event: &str) -> Result<(), String> {
    let path = PathBuf::from(&plan.evidence_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create route evidence dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let body = json!({
        "event": event,
        "recordedAt": Utc::now().to_rfc3339(),
        "driver": "ios_developer_location",
        "doesNotUse3uTools": true,
        "routePlan": plan,
        "backendReadiness": call_ios_dev_location_status(json!({}))?,
    });
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&body)
            .map_err(|e| format!("failed to serialize route evidence: {e}"))?,
    )
    .map_err(|e| format!("failed to write route evidence {}: {e}", path.display()))
}

fn write_route_run_evidence(
    path: &PathBuf,
    response: &Value,
    plan: &IosRoutePlan,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create route run evidence dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let body = json!({
        "event": "route_playback_start",
        "recordedAt": Utc::now().to_rfc3339(),
        "driver": "ios_developer_location",
        "doesNotUse3uTools": true,
        "response": response,
        "routePlan": plan,
        "backendReadiness": call_ios_dev_location_status(json!({}))?,
    });
    std::fs::write(
        path,
        serde_json::to_string_pretty(&body)
            .map_err(|e| format!("failed to serialize route run evidence: {e}"))?,
    )
    .map_err(|e| format!("failed to write route run evidence {}: {e}", path.display()))
}

fn write_route_gpx(plan: &IosRoutePlan, path: &PathBuf) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create route GPX dir {}: {e}", parent.display()))?;
    }
    let started_at = Utc::now();
    let mut body = String::new();
    body.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    body.push('\n');
    body.push_str(
        r#"<gpx version="1.1" creator="Sirin" xmlns="http://www.topografix.com/GPX/1/1">"#,
    );
    body.push('\n');
    body.push_str("  <metadata><name>");
    body.push_str(&xml_escape(&plan.route_id));
    body.push_str("</name></metadata>\n  <trk><name>");
    body.push_str(&xml_escape(&plan.route_id));
    body.push_str("</name><trkseg>\n");
    for (index, point) in plan.scheduled_points.iter().enumerate() {
        let seconds = (index as i64) * (plan.tick_seconds as i64);
        let timestamp = started_at + ChronoDuration::seconds(seconds);
        body.push_str(&format!(
            "    <trkpt lat=\"{:.7}\" lon=\"{:.7}\"><time>{}</time></trkpt>\n",
            point.lat,
            point.lng,
            timestamp.to_rfc3339()
        ));
    }
    body.push_str("  </trkseg></trk>\n</gpx>\n");
    std::fs::write(path, body)
        .map_err(|e| format!("failed to write route GPX {}: {e}", path.display()))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn latest_route_run_id() -> Option<String> {
    let mut entries = std::fs::read_dir(route_runs_dir_path())
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            let id = entry.file_name().to_string_lossy().to_string();
            Some((modified, id))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(modified, _)| *modified);
    entries.pop().map(|(_, id)| id)
}

fn route_run_pid(run_id: &str) -> Option<u64> {
    let evidence_path = route_run_file_path(run_id, "evidence.json");
    let evidence = read_json_file(&evidence_path)?;
    evidence.get("response")?.get("pid")?.as_u64()
}

fn file_stale_seconds(path: &PathBuf) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

fn playback_stalled(
    still_running: Option<bool>,
    completed: bool,
    point_count: usize,
    stale_seconds: Option<u64>,
) -> bool {
    still_running == Some(true)
        && !completed
        && point_count > 0
        && stale_seconds
            .map(|seconds| seconds >= PLAYBACK_STALL_SECONDS)
            .unwrap_or(false)
}

fn analyze_route_playback(
    points: &[IosPoint],
    tick_seconds: u64,
    expected_distance_m: Option<f64>,
    completed: bool,
    stalled: bool,
) -> RoutePlaybackAnalysis {
    let observed_distance_m = route_distance_m(points);
    let speeds = route_step_speeds_kmh(points, tick_seconds);
    let max_step_speed_kmh = speeds.iter().copied().reduce(f64::max);
    let average_speed_kmh = if points.len() > 1 && tick_seconds > 0 {
        let elapsed_seconds = points.len().saturating_sub(1) as f64 * tick_seconds as f64;
        if elapsed_seconds > f64::EPSILON {
            Some(observed_distance_m / elapsed_seconds * 3.6)
        } else {
            None
        }
    } else {
        None
    };
    let overspeed_count = speeds.iter().filter(|speed| **speed > 20.5).count();
    let stop_count = speeds.iter().filter(|speed| **speed < 0.1).count();
    let loop_metrics = route_loop_metrics(points);
    let quality = playback_quality_label(
        completed,
        stalled,
        overspeed_count,
        &loop_metrics,
        observed_distance_m,
        expected_distance_m,
    );

    RoutePlaybackAnalysis {
        quality: quality.into(),
        observed_distance_m: round_meters(observed_distance_m),
        expected_distance_m: expected_distance_m.map(round_meters),
        average_speed_kmh: average_speed_kmh.map(round_speed),
        max_step_speed_kmh: max_step_speed_kmh.map(round_speed),
        overspeed_count,
        stop_count,
        backtrack_count: loop_metrics.backtrack_count,
        repeated_segment_count: loop_metrics.repeated_segment_count,
        overlap_ratio: (loop_metrics.overlap_ratio * 1000.0).round() / 1000.0,
        sample_count: points.len(),
        completed,
        stalled,
    }
}

fn route_step_speeds_kmh(points: &[IosPoint], tick_seconds: u64) -> Vec<f64> {
    if tick_seconds == 0 {
        return Vec::new();
    }
    points
        .windows(2)
        .map(|pair| haversine_m(&pair[0], &pair[1]) / tick_seconds as f64 * 3.6)
        .collect()
}

fn playback_quality_label(
    completed: bool,
    stalled: bool,
    overspeed_count: usize,
    loop_metrics: &RouteLoopMetrics,
    observed_distance_m: f64,
    expected_distance_m: Option<f64>,
) -> &'static str {
    if stalled {
        return "PLAYBACK_STALLED";
    }
    if overspeed_count > 0 {
        return "PLAYBACK_OVERSPEED";
    }
    if !route_loop_acceptable(loop_metrics) {
        return "PLAYBACK_REPEATS_OR_BACKTRACKS";
    }
    if let Some(expected) = expected_distance_m {
        if completed && observed_distance_m + 1.0 < expected * 0.85 {
            return "PLAYBACK_DISTANCE_SHORT";
        }
    }
    if completed {
        "PLAYBACK_GOOD"
    } else {
        "PLAYBACK_RUNNING"
    }
}

fn round_speed(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn read_json_file(path: &PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn parse_playback_set_locations(stderr: &str) -> Vec<IosPoint> {
    stderr
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("set location to ")?;
            let mut parts = rest.split_whitespace();
            let lat = parts.next()?.parse::<f64>().ok()?;
            let lng = parts.next()?.parse::<f64>().ok()?;
            Some(IosPoint { lat, lng })
        })
        .collect()
}

fn text_location_mentions(value: &str) -> Vec<String> {
    value
        .lines()
        .map(str::trim)
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("location")
                || lower.contains("latitude")
                || lower.contains("longitude")
                || lower.contains("gps")
                || lower.contains("geolocation")
        })
        .take(20)
        .map(redact_sensitive_output)
        .collect()
}

fn route_distance_m(points: &[IosPoint]) -> f64 {
    points
        .windows(2)
        .map(|pair| haversine_m(&pair[0], &pair[1]))
        .sum()
}

fn sample_route_points(
    points: &[IosPoint],
    speed_kmh: f64,
    duration_seconds: u64,
    tick_seconds: u64,
    route_distance_m: f64,
    movement_profile: &str,
) -> Vec<IosPoint> {
    if movement_profile.eq_ignore_ascii_case("bicycle") {
        return sample_bicycle_route_points(
            points,
            speed_kmh,
            duration_seconds,
            tick_seconds,
            route_distance_m,
        );
    }
    let tick_count = duration_seconds / tick_seconds;
    let meters_per_second = speed_kmh * 1000.0 / 3600.0;
    let max_step_m = MAX_ROUTE_STEP_SPEED_KMH * 1000.0 / 3600.0 * tick_seconds as f64;
    let base_step_m = meters_per_second * tick_seconds as f64;
    let mut distance = 0.0;
    let mut sampled = Vec::with_capacity(tick_count as usize + 1);
    sampled.push(point_at_distance(points, 0.0));
    for tick in 1..=tick_count {
        let factor = ROUTE_SPEED_VARIATION_FACTORS
            [(tick as usize - 1) % ROUTE_SPEED_VARIATION_FACTORS.len()];
        let step_m = (base_step_m * factor).min(max_step_m).max(0.0);
        distance = (distance + step_m).min(route_distance_m);
        sampled.push(point_at_distance(points, distance));
    }
    sampled
}

fn sample_bicycle_route_points(
    points: &[IosPoint],
    speed_kmh: f64,
    duration_seconds: u64,
    tick_seconds: u64,
    route_distance_m: f64,
) -> Vec<IosPoint> {
    let tick_count = duration_seconds / tick_seconds;
    let base_speed_kmh = speed_kmh.min(20.0).max(1.0);
    let max_step_m = MAX_ROUTE_STEP_SPEED_KMH * 1000.0 / 3600.0 * tick_seconds as f64;
    let geometry = BicycleRouteGeometry::from_points(points);
    let mut distance = 0.0;
    let mut sampled = Vec::with_capacity(tick_count as usize + 1);
    sampled.push(point_at_distance(points, 0.0));
    for tick in 1..=tick_count {
        let speed_kmh = bicycle_tick_speed_kmh(tick, base_speed_kmh, distance, &geometry);
        let step_m = (speed_kmh * 1000.0 / 3600.0 * tick_seconds as f64)
            .min(max_step_m)
            .max(0.0);
        distance = (distance + step_m).min(route_distance_m);
        sampled.push(point_at_distance(points, distance));
    }
    sampled
}

fn bicycle_tick_speed_kmh(
    tick: u64,
    base_speed_kmh: f64,
    current_distance_m: f64,
    geometry: &BicycleRouteGeometry,
) -> f64 {
    if tick <= 2 || (tick >= 118 && tick <= 123) || (tick >= 274 && tick <= 280) {
        return 0.0;
    }
    let startup = if tick < 24 {
        0.28 + (tick as f64 / 24.0) * 0.72
    } else {
        1.0
    };
    let turn_slowdown = match tick % 90 {
        0..=4 => 0.48,
        5..=9 => 0.68,
        10..=14 => 0.84,
        _ => 1.0,
    };
    let geometric_slowdown = geometry.slowdown_factor(current_distance_m);
    let factor = BICYCLE_SPEED_VARIATION_FACTORS
        [(tick as usize - 1) % BICYCLE_SPEED_VARIATION_FACTORS.len()];
    (base_speed_kmh * startup * turn_slowdown * geometric_slowdown * factor)
        .clamp(0.0, MAX_ROUTE_STEP_SPEED_KMH)
}

#[derive(Debug, Clone)]
struct BicycleRouteGeometry {
    turns: Vec<BicycleTurn>,
}

#[derive(Debug, Clone)]
struct BicycleTurn {
    distance_m: f64,
    angle_deg: f64,
}

impl BicycleRouteGeometry {
    fn from_points(points: &[IosPoint]) -> Self {
        if points.len() < 3 {
            return Self { turns: Vec::new() };
        }
        let mut cumulative = Vec::with_capacity(points.len());
        cumulative.push(0.0);
        for pair in points.windows(2) {
            let next = cumulative.last().copied().unwrap_or(0.0) + haversine_m(&pair[0], &pair[1]);
            cumulative.push(next);
        }
        let turns = points
            .windows(3)
            .enumerate()
            .filter_map(|(idx, triple)| {
                let angle_deg = turn_angle_deg(&triple[0], &triple[1], &triple[2]);
                if angle_deg < 28.0 {
                    return None;
                }
                Some(BicycleTurn {
                    distance_m: cumulative[idx + 1],
                    angle_deg,
                })
            })
            .collect();
        Self { turns }
    }

    fn slowdown_factor(&self, current_distance_m: f64) -> f64 {
        self.turns
            .iter()
            .filter_map(|turn| {
                let distance_to_turn = (turn.distance_m - current_distance_m).abs();
                if distance_to_turn > BICYCLE_TURN_LOOKAHEAD_M {
                    return None;
                }
                let proximity = 1.0 - (distance_to_turn / BICYCLE_TURN_LOOKAHEAD_M);
                let angle_weight = (turn.angle_deg / 120.0).clamp(0.25, 1.0);
                let slowdown = 1.0 - (0.48 * proximity * angle_weight);
                Some(slowdown.clamp(0.45, 1.0))
            })
            .fold(1.0, f64::min)
    }
}

fn turn_angle_deg(prev: &IosPoint, current: &IosPoint, next: &IosPoint) -> f64 {
    let v1x = current.lng - prev.lng;
    let v1y = current.lat - prev.lat;
    let v2x = next.lng - current.lng;
    let v2y = next.lat - current.lat;
    let len1 = (v1x * v1x + v1y * v1y).sqrt();
    let len2 = (v2x * v2x + v2y * v2y).sqrt();
    if len1 <= f64::EPSILON || len2 <= f64::EPSILON {
        return 0.0;
    }
    let dot = (v1x * v2x + v1y * v2y) / (len1 * len2);
    dot.clamp(-1.0, 1.0).acos().to_degrees()
}

fn point_at_distance(points: &[IosPoint], target_m: f64) -> IosPoint {
    if target_m <= 0.0 {
        return points[0].clone();
    }
    let mut remaining = target_m;
    for pair in points.windows(2) {
        let segment_m = haversine_m(&pair[0], &pair[1]);
        if segment_m <= f64::EPSILON {
            continue;
        }
        if remaining <= segment_m {
            return interpolate_point(&pair[0], &pair[1], remaining / segment_m);
        }
        remaining -= segment_m;
    }
    points.last().cloned().unwrap_or_else(|| points[0].clone())
}

fn interpolate_point(start: &IosPoint, end: &IosPoint, ratio: f64) -> IosPoint {
    let clamped = ratio.clamp(0.0, 1.0);
    IosPoint {
        lat: start.lat + (end.lat - start.lat) * clamped,
        lng: start.lng + (end.lng - start.lng) * clamped,
    }
}

fn haversine_m(a: &IosPoint, b: &IosPoint) -> f64 {
    let lat1 = a.lat.to_radians();
    let lat2 = b.lat.to_radians();
    let dlat = (b.lat - a.lat).to_radians();
    let dlng = (b.lng - a.lng).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

fn route_preview(points: &[IosPoint]) -> Value {
    let first = points.iter().take(5).cloned().collect::<Vec<_>>();
    let mut last = points.iter().rev().take(5).cloned().collect::<Vec<_>>();
    last.reverse();
    json!({
        "first": first,
        "last": last
    })
}

fn round_meters(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn device_info_summary(path: &PathBuf) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return json!({"available": false});
    };
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let latest = lines.pop().unwrap_or("");
    let latest_seen_at = latest.split('\t').next().unwrap_or("").to_string();
    json!({
        "available": true,
        "recordCount": lines.len() + usize::from(!latest.is_empty()),
        "latestSeenAt": latest_seen_at,
        "identifiersRedacted": true
    })
}

fn sanitize_file_part(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "ios_location".into()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn excerpt(value: &str, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(max_chars).collect())
    }
}

fn redact_sensitive_output(value: &str) -> String {
    value
        .split_whitespace()
        .map(redact_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn redact_token(token: &str) -> String {
    let core = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let normalized_hex = core.replace('-', "");
    let is_long_hex = normalized_hex.len() >= 16
        && normalized_hex.chars().all(|c| c.is_ascii_hexdigit())
        && core.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    let is_long_digits = core.len() >= 12 && core.chars().all(|c| c.is_ascii_digit());
    if is_long_hex || is_long_digits {
        token.replace(core, "[redacted-id]")
    } else {
        token.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_route_plan(route_id: &str, created_at: &str) -> IosRoutePlan {
        IosRoutePlan {
            route_id: route_id.into(),
            status: "READY_REQUIRES_REAL_IPHONE".into(),
            created_at: created_at.into(),
            mode: "road_route".into(),
            city: Some("chiang_mai".into()),
            center: Some(IosPoint {
                lat: 18.788333,
                lng: 98.985299,
            }),
            radius_m: Some(1500.0),
            route_source: "test".into(),
            road_provider: Some("osrm".into()),
            road_routed: true,
            movement_profile: "roam".into(),
            speed_kmh: 20.0,
            duration_seconds: 2,
            tick_seconds: 1,
            requested_distance_m: 11.11,
            route_distance_m: 11.11,
            detour_ratio: Some(1.0),
            route_quality: "ROAD_ROUTE_GOOD".into(),
            route_playable: true,
            route_block_reason: None,
            route_loop_metrics: RouteLoopMetrics::default(),
            snap_offset_m: Some(0.0),
            seed_waypoints: vec![IosPoint {
                lat: 18.788333,
                lng: 98.985299,
            }],
            road_route_points: vec![IosPoint {
                lat: 18.788350,
                lng: 98.985320,
            }],
            scheduled_points: vec![
                IosPoint {
                    lat: 18.788333,
                    lng: 98.985299,
                },
                IosPoint {
                    lat: 18.788400,
                    lng: 98.985350,
                },
            ],
            evidence_path: "unused".into(),
            note: None,
        }
    }

    #[test]
    fn point_accepts_lat_lng_and_rejects_invalid_range() {
        let point = point_from_args(&json!({"lat": 25.033964, "lng": 121.564468})).unwrap();
        assert_eq!(point.lat, 25.033964);
        assert_eq!(point.lng, 121.564468);
        assert!(point_from_args(&json!({"lat": 91.0, "lng": 121.0})).is_err());
        assert!(point_from_args(&json!({"lat": 25.0, "lng": 181.0})).is_err());
    }

    #[test]
    fn location_report_serializes_camel_case_fields() {
        let report = IosLocationReport {
            report_id: "IOSLOCREPORT-test".into(),
            status: "REPORT_RECEIVED".into(),
            lat: 18.788343,
            lng: 98.985300,
            accuracy_meters: Some(12.0),
            source: "ios_browser_geolocation".into(),
            user_agent: Some("Mobile Safari".into()),
            reported_at: Some("2026-07-09T00:00:00Z".into()),
            received_at: "2026-07-09T00:00:01Z".into(),
            evidence_path: "unused".into(),
            note: None,
        };
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["reportId"], "IOSLOCREPORT-test");
        assert_eq!(value["accuracyMeters"], 12.0);
        assert_eq!(value["reportedAt"], "2026-07-09T00:00:00Z");
        assert_eq!(value["receivedAt"], "2026-07-09T00:00:01Z");
    }

    #[test]
    fn confirm_status_normalizes_common_values() {
        assert_eq!(normalize_confirm_status("success"), "APPLIED");
        assert_eq!(normalize_confirm_status("restore_done"), "RESTORED");
        assert_eq!(normalize_confirm_status("error"), "FAILED");
        assert_eq!(normalize_confirm_status("other"), "OPERATOR_CONFIRMED");
    }

    #[test]
    fn sanitize_file_part_keeps_safe_chars() {
        assert_eq!(sanitize_file_part("IOS:1/tick 2"), "IOS_1_tick_2");
    }

    #[test]
    fn device_info_summary_redacts_identifiers() {
        let path = std::env::temp_dir().join(format!(
            "sirin_ios_deviceinfo_{}.txt",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::write(
            &path,
            "2026-05-09 12:00:27\tUDIDSECRET\tSERIALSECRET\tIMEISECRET\r\n",
        )
        .unwrap();
        let summary = device_info_summary(&path);
        let text = summary.to_string();
        let _ = std::fs::remove_file(&path);
        assert!(summary["identifiersRedacted"].as_bool().unwrap());
        assert!(!text.contains("UDIDSECRET"));
        assert!(!text.contains("SERIALSECRET"));
        assert!(!text.contains("IMEISECRET"));
    }

    #[test]
    fn redact_sensitive_output_masks_device_identifiers() {
        let text = redact_sensitive_output("000081500016743C14E2401C 358418981892336 ok");
        assert!(!text.contains("000081500016743C14E2401C"));
        assert!(!text.contains("358418981892336"));
        assert!(text.contains("[redacted-id]"));
        assert!(text.contains("ok"));
    }

    #[test]
    fn redact_sensitive_output_masks_hyphenated_udid() {
        let text = redact_sensitive_output(
            "\"Identifier\": \"00008150-0016743C14E2401C\", \"DeviceClass\": \"iPhone\"",
        );
        assert!(!text.contains("00008150-0016743C14E2401C"));
        assert!(text.contains("[redacted-id]"));
        assert!(text.contains("iPhone"));
    }

    #[test]
    fn cli_failure_detection_catches_pymobiledevice_no_device() {
        assert!(looks_like_cli_failure(
            "",
            "2026-07-08 host pymobiledevice3.__main__[17008] ERROR Device is not connected"
        ));
        assert!(!looks_like_cli_failure("", "location simulation started"));
    }

    #[test]
    fn cli_ambiguous_detection_catches_userspace_prompt_abort() {
        assert!(looks_like_cli_ambiguous("Press ENTER to exit>", "Aborted."));
        assert!(!looks_like_cli_failure("Press ENTER to exit>", "Aborted."));
    }

    #[test]
    fn pymobiledevice3_device_count_treats_empty_array_as_no_device() {
        assert_eq!(pymobiledevice3_device_count("[]"), 0);
        assert_eq!(
            pymobiledevice3_device_count(
                r#"[{"DeviceName":"Redan-iPhone 17","ConnectionType":"USB"}]"#
            ),
            1
        );
    }

    #[test]
    fn playback_log_parser_extracts_set_location_points() {
        let points = parse_playback_set_locations(
            "2026-07-09 LocationSimulation INFO set location to 18.788333 98.985299\n\
             2026-07-09 LocationSimulation INFO waiting for 1.000s\n\
             2026-07-09 LocationSimulation INFO set location to 18.7883517 98.9850359",
        );
        assert_eq!(points.len(), 2);
        assert!((points[0].lat - 18.788333).abs() < 0.000001);
        assert!((points[1].lng - 98.9850359).abs() < 0.000001);
        assert!(route_distance_m(&points) > 20.0);
    }

    #[test]
    fn playback_stalled_requires_running_incomplete_and_stale_log() {
        assert!(playback_stalled(
            Some(true),
            false,
            12,
            Some(PLAYBACK_STALL_SECONDS)
        ));
        assert!(!playback_stalled(
            Some(true),
            false,
            12,
            Some(PLAYBACK_STALL_SECONDS - 1)
        ));
        assert!(!playback_stalled(
            Some(false),
            false,
            12,
            Some(PLAYBACK_STALL_SECONDS)
        ));
        assert!(!playback_stalled(
            Some(true),
            true,
            12,
            Some(PLAYBACK_STALL_SECONDS)
        ));
        assert!(!playback_stalled(
            Some(true),
            false,
            0,
            Some(PLAYBACK_STALL_SECONDS)
        ));
    }

    #[test]
    fn playback_analysis_marks_clean_completed_route_good() {
        let points = vec![
            IosPoint {
                lat: 18.795300,
                lng: 98.998900,
            },
            IosPoint {
                lat: 18.795350,
                lng: 98.998950,
            },
            IosPoint {
                lat: 18.795400,
                lng: 98.999000,
            },
        ];
        let analysis = analyze_route_playback(&points, 2, Some(10.0), true, false);
        assert_eq!(analysis.quality, "PLAYBACK_GOOD");
        assert_eq!(analysis.overspeed_count, 0);
        assert_eq!(analysis.backtrack_count, 0);
    }

    #[test]
    fn playback_analysis_detects_overspeed() {
        let points = vec![
            IosPoint {
                lat: 18.795300,
                lng: 98.998900,
            },
            IosPoint {
                lat: 18.796300,
                lng: 98.998900,
            },
        ];
        let analysis = analyze_route_playback(&points, 1, Some(100.0), true, false);
        assert_eq!(analysis.quality, "PLAYBACK_OVERSPEED");
        assert!(analysis.overspeed_count > 0);
        assert!(analysis.max_step_speed_kmh.unwrap() > 20.5);
    }

    #[test]
    fn playback_analysis_detects_repeat_or_backtrack() {
        let points = vec![
            IosPoint {
                lat: 18.795300,
                lng: 98.998900,
            },
            IosPoint {
                lat: 18.795900,
                lng: 98.998900,
            },
            IosPoint {
                lat: 18.795300,
                lng: 98.998900,
            },
        ];
        let analysis = analyze_route_playback(&points, 60, Some(100.0), true, false);
        assert_eq!(analysis.quality, "PLAYBACK_REPEATS_OR_BACKTRACKS");
        assert!(analysis.backtrack_count > 0);
        assert!(analysis.repeated_segment_count > 0);
    }

    #[test]
    fn route_plan_samples_20kmh_for_10_minutes() {
        let points = vec![
            IosPoint {
                lat: 25.033964,
                lng: 121.564468,
            },
            IosPoint {
                lat: 25.064000,
                lng: 121.564468,
            },
            IosPoint {
                lat: 25.064000,
                lng: 121.604468,
            },
        ];
        let distance = route_distance_m(&points);
        let sampled = sample_route_points(&points, 20.0, 600, 1, distance, "roam");
        assert_eq!(sampled.len(), 601);
        assert_ne!(sampled.first().unwrap().lat, sampled.last().unwrap().lat);
    }

    #[test]
    fn route_plan_varies_speed_without_exceeding_cap() {
        let points = vec![
            IosPoint {
                lat: 25.033964,
                lng: 121.564468,
            },
            IosPoint {
                lat: 25.090000,
                lng: 121.564468,
            },
        ];
        let distance = route_distance_m(&points);
        let sampled = sample_route_points(&points, 20.0, 60, 1, distance, "roam");
        let speeds = sampled
            .windows(2)
            .map(|pair| haversine_m(&pair[0], &pair[1]) * 3.6)
            .collect::<Vec<_>>();
        let max_speed = speeds.iter().copied().fold(0.0, f64::max);
        let min_speed = speeds.iter().copied().fold(f64::MAX, f64::min);
        assert!(
            max_speed <= MAX_ROUTE_STEP_SPEED_KMH + 0.05,
            "max_speed={max_speed}"
        );
        assert!(max_speed - min_speed > 0.1, "speed should vary");
    }

    #[test]
    fn route_loop_metrics_allow_one_way_path() {
        let points = vec![
            IosPoint {
                lat: 18.788300,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.789000,
                lng: 98.986000,
            },
            IosPoint {
                lat: 18.789700,
                lng: 98.987200,
            },
        ];
        let metrics = route_loop_metrics(&points);
        assert_eq!(metrics.backtrack_count, 0);
        assert_eq!(metrics.repeated_segment_count, 0);
        assert!(route_loop_acceptable(&metrics));
    }

    #[test]
    fn route_loop_metrics_detect_immediate_backtrack() {
        let points = vec![
            IosPoint {
                lat: 18.788300,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.789000,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.788300,
                lng: 98.985300,
            },
        ];
        let metrics = route_loop_metrics(&points);
        assert!(metrics.backtrack_count > 0);
        assert!(metrics.repeated_segment_count > 0);
        assert!(!route_loop_acceptable(&metrics));
    }

    #[test]
    fn route_quality_marks_repeat_or_backtrack() {
        let points = vec![
            IosPoint {
                lat: 18.788300,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.789000,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.788300,
                lng: 98.985300,
            },
        ];
        let metrics = route_loop_metrics(&points);
        assert_eq!(
            route_quality_label(true, Some(1.0), Some(0.0), &metrics),
            "ROAD_REPEATS_OR_BACKTRACKS"
        );
    }

    #[test]
    fn route_plan_gate_rejects_backtrack_before_playback() {
        let metrics = RouteLoopMetrics {
            backtrack_count: 1,
            repeated_segment_count: 1,
            overlap_ratio: 0.25,
        };
        let gate = route_plan_gate(1000.0, 800.0, "ROAD_REPEATS_OR_BACKTRACKS", &metrics);
        assert_eq!(gate.status, "ROUTE_REJECTED_REGENERATE_REQUIRED");
        assert!(!gate.playable);
        assert!(gate.block_reason.unwrap().contains("backtracks"));
    }

    #[test]
    fn route_plan_gate_accepts_clean_route_with_enough_distance() {
        let metrics = RouteLoopMetrics::default();
        let gate = route_plan_gate(1000.0, 800.0, "ROAD_ROUTE_GOOD", &metrics);
        assert_eq!(gate.status, "READY_REQUIRES_REAL_IPHONE");
        assert!(gate.playable);
        assert!(gate.block_reason.is_none());
    }

    #[test]
    fn route_choice_score_prefers_sufficient_route_over_short_route() {
        let short = IosRoamRouteChoice {
            seed_waypoints: vec![],
            points: vec![],
            road_routed: true,
            radius_m: 100.0,
            detour_ratio: Some(0.05),
            route_loop_metrics: RouteLoopMetrics::default(),
            route_quality: "ROAD_ROUTE_GOOD".into(),
        };
        let sufficient = IosRoamRouteChoice {
            seed_waypoints: vec![],
            points: vec![],
            road_routed: true,
            radius_m: 900.0,
            detour_ratio: Some(1.25),
            route_loop_metrics: RouteLoopMetrics::default(),
            route_quality: "ROAD_ROUTE_GOOD".into(),
        };
        assert!(route_choice_score(&sufficient) < route_choice_score(&short));
    }

    #[test]
    fn route_plan_summary_omits_large_point_arrays() {
        let plan = sample_route_plan("IOSROUTE-summary", "2026-07-09T00:00:00Z");
        let summary = route_plan_summary(&plan);
        assert_eq!(summary["routeId"], "IOSROUTE-summary");
        assert_eq!(summary["seedWaypointCount"], 1);
        assert_eq!(summary["roadRoutePointCount"], 1);
        assert_eq!(summary["scheduledPointCount"], 2);
        assert!(summary.get("seedWaypoints").is_none());
        assert!(summary.get("roadRoutePoints").is_none());
        assert!(summary.get("scheduledPoints").is_none());
    }

    #[test]
    fn prune_route_plans_keeps_latest_plans_under_memory_cap() {
        let mut plans = HashMap::new();
        for index in 0..(MAX_ROUTE_PLANS_IN_MEMORY + 3) {
            let route_id = format!("IOSROUTE-{index:02}");
            let created_at = format!("2026-07-09T00:00:{index:02}Z");
            plans.insert(route_id.clone(), sample_route_plan(&route_id, &created_at));
        }
        prune_route_plans(&mut plans);
        assert_eq!(plans.len(), MAX_ROUTE_PLANS_IN_MEMORY);
        assert!(!plans.contains_key("IOSROUTE-00"));
        assert!(!plans.contains_key("IOSROUTE-01"));
        assert!(!plans.contains_key("IOSROUTE-02"));
        assert!(plans.contains_key("IOSROUTE-03"));
        assert!(plans.contains_key("IOSROUTE-12"));
    }

    #[test]
    fn roam_seed_waypoints_progress_away_from_center() {
        let center = IosPoint {
            lat: 18.79535,
            lng: 98.99895,
        };
        let points = roam_seed_waypoints(&center, 900.0, 8);
        assert_eq!(points.len(), 9);
        let first_leg = haversine_m(&center, &points[1]);
        let last_leg = haversine_m(&center, points.last().unwrap());
        assert!(first_leg < 250.0, "first_leg={first_leg}");
        assert!(last_leg > 800.0, "last_leg={last_leg}");
    }

    #[test]
    fn route_plan_bicycle_profile_has_stops_variation_and_speed_cap() {
        let points = vec![
            IosPoint {
                lat: 18.788343,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.788343,
                lng: 99.035300,
            },
        ];
        let distance = route_distance_m(&points);
        let sampled = sample_route_points(&points, 20.0, 300, 1, distance, "bicycle");
        let speeds = sampled
            .windows(2)
            .map(|pair| haversine_m(&pair[0], &pair[1]) * 3.6)
            .collect::<Vec<_>>();
        let max_speed = speeds.iter().copied().fold(0.0, f64::max);
        let stop_count = speeds.iter().filter(|speed| **speed < 0.1).count();
        let slow_count = speeds
            .iter()
            .filter(|speed| **speed >= 0.1 && **speed < 12.0)
            .count();
        assert!(
            max_speed <= MAX_ROUTE_STEP_SPEED_KMH + 0.05,
            "max_speed={max_speed}"
        );
        assert!(stop_count >= 6, "stop_count={stop_count}");
        assert!(slow_count >= 10, "slow_count={slow_count}");
    }

    #[test]
    fn bicycle_profile_slows_near_route_geometry_turns() {
        let straight = vec![
            IosPoint {
                lat: 18.788343,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.788343,
                lng: 99.015300,
            },
        ];
        let turning = vec![
            IosPoint {
                lat: 18.788343,
                lng: 98.985300,
            },
            IosPoint {
                lat: 18.788343,
                lng: 98.986000,
            },
            IosPoint {
                lat: 18.789043,
                lng: 98.986000,
            },
            IosPoint {
                lat: 18.789043,
                lng: 99.015300,
            },
        ];
        let straight_sampled = sample_route_points(
            &straight,
            20.0,
            45,
            1,
            route_distance_m(&straight),
            "bicycle",
        );
        let turning_sampled =
            sample_route_points(&turning, 20.0, 45, 1, route_distance_m(&turning), "bicycle");
        let straight_distance = route_distance_m(&straight_sampled);
        let turning_distance = route_distance_m(&turning_sampled);
        assert!(
            turning_distance + 15.0 < straight_distance,
            "turning_distance={turning_distance}, straight_distance={straight_distance}"
        );
        let geometry = BicycleRouteGeometry::from_points(&turning);
        assert!(!geometry.turns.is_empty());
        let turn_distance = geometry.turns[0].distance_m;
        assert!(geometry.slowdown_factor(turn_distance) < 0.8);
    }

    #[test]
    fn route_gpx_contains_scheduled_points_and_timestamps() {
        let plan = sample_route_plan("IOSROUTE-test", &Utc::now().to_rfc3339());
        let path = std::env::temp_dir().join(format!(
            "sirin-ios-route-gpx-test-{}.gpx",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        write_route_gpx(&plan, &path).unwrap();
        let gpx = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(gpx.contains("<trkpt lat=\"18.7883330\" lon=\"98.9852990\">"));
        assert!(gpx.contains("<trkpt lat=\"18.7884000\" lon=\"98.9853500\">"));
        assert!(gpx.contains("<time>"));
    }

    #[test]
    fn route_points_require_road_polyline() {
        assert!(route_points_from_args(&json!({
            "route": {
                "start": {"lat": 25.0, "lng": 121.0},
                "end": {"lat": 25.1, "lng": 121.1}
            }
        }))
        .is_err());
        assert!(route_points_from_args(&json!({
            "points": [
                {"lat": 25.0, "lng": 121.0},
                {"lat": 25.1, "lng": 121.1}
            ]
        }))
        .is_ok());
    }

    #[test]
    fn roam_plan_does_not_fallback_to_taipei_without_location() {
        let err = ios_roam_route_from_args(&json!({
            "roadProvider": "synthetic"
        }))
        .unwrap_err();
        assert!(err.contains("refusing to fallback to Taipei"));
    }

    #[test]
    fn roam_plan_accepts_chiang_mai_city_center() {
        let route = ios_roam_route_from_args(&json!({
            "city": "chiang_mai",
            "roadProvider": "synthetic"
        }))
        .unwrap();
        assert_eq!(route.city.as_deref(), Some("chiang_mai"));
        let center = route.center.unwrap();
        assert!((center.lat - 18.788343).abs() < 0.000001);
        assert!((center.lng - 98.985300).abs() < 0.000001);
    }
}
