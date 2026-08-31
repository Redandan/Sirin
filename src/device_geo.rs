use crate::platform::NoWindow;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread;
use std::time::Duration;

const EARTH_RADIUS_M: f64 = 6_371_000.0;
const DEFAULT_SPEED_KMH: f64 = 20.0;
const DEFAULT_DURATION_SECONDS: u64 = 600;
const DEFAULT_TICK_SECONDS: u64 = 1;
const DEFAULT_AVD: &str = "Sirin_Pixel_7_API_35";
const DEFAULT_ROAM_RADIUS_M: f64 = 1500.0;
const DEFAULT_ROAD_PROVIDER: &str = "osrm";
const DEFAULT_OSRM_BASE_URL: &str = "https://router.project-osrm.org";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeoPoint {
    lat: f64,
    lng: f64,
}

#[derive(Debug, Clone, Serialize)]
struct RouteRunState {
    route_id: String,
    device: String,
    status: String,
    started_at: String,
    updated_at: String,
    speed_kmh: f64,
    duration_seconds: u64,
    tick_seconds: u64,
    total_points: usize,
    current_index: usize,
    last_point: Option<GeoPoint>,
    last_error: Option<String>,
    screen_monitor_enabled: bool,
    screen_sample_every_ticks: usize,
    screen_require_change: bool,
    screen_samples: usize,
    screen_stale_ticks: usize,
    screen_max_stale_ticks: usize,
    last_screenshot_path: Option<String>,
    last_screenshot_sha256: Option<String>,
    last_screen_changed: Option<bool>,
    screen_evidence: Vec<ScreenEvidence>,
}

struct RouteRun {
    state: Arc<Mutex<RouteRunState>>,
    stop: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize)]
struct LoginWaitState {
    login_id: String,
    device: String,
    package: Option<String>,
    status: String,
    started_at: String,
    updated_at: String,
    timeout_seconds: u64,
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ScreenshotCapture {
    path: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize)]
struct ScreenEvidence {
    tick: usize,
    captured_at: String,
    path: String,
    size_bytes: u64,
    sha256: String,
    changed: Option<bool>,
}

#[derive(Debug, Clone)]
struct ScreenMonitorConfig {
    enabled: bool,
    sample_every_ticks: usize,
    require_change: bool,
    max_stale_ticks: usize,
}

static ROUTES: OnceLock<Mutex<HashMap<String, RouteRun>>> = OnceLock::new();
static LOGIN_WAITS: OnceLock<Mutex<HashMap<String, LoginWaitState>>> = OnceLock::new();

fn routes() -> &'static Mutex<HashMap<String, RouteRun>> {
    ROUTES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn login_waits() -> &'static Mutex<HashMap<String, LoginWaitState>> {
    LOGIN_WAITS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn call_device_emulator_start(args: Value) -> Result<Value, String> {
    let avd = arg_text(&args, "avd").unwrap_or_else(|| DEFAULT_AVD.into());
    let timeout_seconds = u64_param(&args, "timeoutSeconds", 180)
        .or_else(|| u64_param(&args, "timeout_seconds", 180))
        .unwrap_or(180)
        .clamp(1, 600);
    let no_window = bool_param(&args, "noWindow", true);
    let gpu = arg_text(&args, "gpu").unwrap_or_else(|| "swiftshader_indirect".into());

    let adb = adb_path()?;
    if let Some(device) = first_ready_emulator(&adb)? {
        return Ok(json!({
            "status": "already_running",
            "device": device,
            "avd": avd,
            "bootCompleted": boot_completed(&adb, &device).unwrap_or(false)
        }));
    }

    let emulator = emulator_path()?;
    let mut cmd = Command::new(&emulator);
    cmd.no_window();
    cmd.arg("-avd")
        .arg(&avd)
        .arg("-no-audio")
        .arg("-no-boot-anim")
        .arg("-no-snapshot")
        .arg("-gpu")
        .arg(&gpu)
        .env("ANDROID_HOME", android_sdk_root().unwrap_or_default())
        .env("ANDROID_SDK_ROOT", android_sdk_root().unwrap_or_default());
    if let Some(avd_home) = android_avd_home() {
        cmd.env("ANDROID_AVD_HOME", avd_home);
    }
    if no_window {
        cmd.arg("-no-window");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to start emulator {}: {e}", emulator.display()))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds);
    while std::time::Instant::now() < deadline {
        if let Some(device) = first_ready_emulator(&adb)? {
            if boot_completed(&adb, &device).unwrap_or(false) {
                return Ok(json!({
                    "status": "started",
                    "device": device,
                    "avd": avd,
                    "bootCompleted": true,
                    "androidSdkRoot": android_sdk_root().map(|p| p.display().to_string()).unwrap_or_default(),
                    "androidAvdHome": android_avd_home().map(|p| p.display().to_string()).unwrap_or_default()
                }));
            }
        }
        thread::sleep(Duration::from_secs(3));
    }

    Ok(json!({
        "status": "starting",
        "avd": avd,
        "bootCompleted": false,
        "message": "emulator process was launched but boot did not complete before timeout"
    }))
}

pub(crate) fn call_device_emulator_status(_args: Value) -> Result<Value, String> {
    let adb = adb_path()?;
    let devices = adb_devices(&adb)?;
    let mut enriched = Vec::new();
    for mut device in devices {
        if let Some(serial) = device
            .get("serial")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            device["bootCompleted"] = json!(boot_completed(&adb, &serial).unwrap_or(false));
            device["foregroundPackage"] = json!(foreground_package(&adb, &serial).ok().flatten());
        }
        enriched.push(device);
    }
    Ok(json!({
        "androidSdkRoot": android_sdk_root().map(|p| p.display().to_string()).unwrap_or_default(),
        "androidAvdHome": android_avd_home().map(|p| p.display().to_string()).unwrap_or_default(),
        "avds": emulator_list_avds().unwrap_or_default(),
        "devices": enriched
    }))
}

pub(crate) fn call_device_app_launch(args: Value) -> Result<Value, String> {
    let device = selected_device(&args)?;
    ensure_emulator_allowed(&device, &args)?;
    let package = arg_text(&args, "package").ok_or_else(|| "package is required".to_string())?;
    let adb = adb_path()?;
    let output = if let Some(activity) = arg_text(&args, "activity") {
        Command::new(&adb)
            .no_window()
            .args(["-s", &device, "shell", "am", "start", "-n"])
            .arg(format!("{package}/{activity}"))
            .output()
            .map_err(|e| format!("adb am start failed: {e}"))?
    } else {
        Command::new(&adb)
            .no_window()
            .args([
                "-s",
                &device,
                "shell",
                "monkey",
                "-p",
                &package,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ])
            .output()
            .map_err(|e| format!("adb monkey launch failed: {e}"))?
    };
    if !output.status.success() {
        return Err(format!(
            "app launch failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(json!({
        "status": "launched",
        "device": device,
        "package": package,
        "stdout": String::from_utf8_lossy(&output.stdout).trim(),
        "stderr": String::from_utf8_lossy(&output.stderr).trim()
    }))
}

pub(crate) fn call_device_operator_login_start(args: Value) -> Result<Value, String> {
    let device = selected_device(&args)?;
    ensure_emulator_allowed(&device, &args)?;
    let login_id = arg_text(&args, "loginId")
        .or_else(|| arg_text(&args, "login_id"))
        .unwrap_or_else(|| format!("LOGIN-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let timeout_seconds = u64_param(&args, "timeoutSeconds", 300)
        .or_else(|| u64_param(&args, "timeout_seconds", 300))
        .unwrap_or(300)
        .clamp(1, 3600);
    let now = Utc::now().to_rfc3339();
    let state = LoginWaitState {
        login_id: login_id.clone(),
        device: device.clone(),
        package: arg_text(&args, "package"),
        status: "WAITING_FOR_OPERATOR_LOGIN".into(),
        started_at: now.clone(),
        updated_at: now,
        timeout_seconds,
        note: arg_text(&args, "note"),
    };
    login_waits()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(login_id.clone(), state.clone());
    Ok(json!({
        "status": "WAITING_FOR_OPERATOR_LOGIN",
        "loginId": login_id,
        "device": device,
        "package": state.package,
        "timeoutSeconds": timeout_seconds,
        "operatorAction": "Log in manually in the emulator, then call device_operator_login_confirm."
    }))
}

pub(crate) fn call_device_operator_login_status(args: Value) -> Result<Value, String> {
    let login_id = arg_text(&args, "loginId").or_else(|| arg_text(&args, "login_id"));
    let guard = login_waits().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = login_id {
        let state = guard
            .get(&id)
            .ok_or_else(|| format!("login wait not found: {id}"))?;
        return Ok(json!({ "login": state }));
    }
    Ok(json!({ "logins": guard.values().collect::<Vec<_>>() }))
}

pub(crate) fn call_device_operator_login_confirm(args: Value) -> Result<Value, String> {
    let login_id = arg_text(&args, "loginId")
        .or_else(|| arg_text(&args, "login_id"))
        .ok_or_else(|| "loginId is required".to_string())?;
    let mut guard = login_waits().lock().unwrap_or_else(|e| e.into_inner());
    let state = guard
        .get_mut(&login_id)
        .ok_or_else(|| format!("login wait not found: {login_id}"))?;
    state.status = "OPERATOR_LOGIN_CONFIRMED".into();
    state.updated_at = Utc::now().to_rfc3339();
    state.note = arg_text(&args, "note").or_else(|| state.note.clone());
    Ok(json!({ "status": "OPERATOR_LOGIN_CONFIRMED", "login": state }))
}

pub(crate) fn call_device_list(_args: Value) -> Result<Value, String> {
    let adb = adb_path()?;
    let devices = adb_devices(&adb)?;
    let avds = emulator_list_avds().unwrap_or_default();
    Ok(json!({
        "androidSdkRoot": android_sdk_root().map(|p| p.display().to_string()).unwrap_or_default(),
        "adbPath": adb.display().to_string(),
        "devices": devices,
        "avds": avds,
        "safety": {
            "physicalDevicesBlockedByDefault": true,
            "allowedDevicePattern": "emulator-*"
        }
    }))
}

pub(crate) fn call_device_screenshot(args: Value) -> Result<Value, String> {
    let device = selected_device(&args)?;
    ensure_emulator_allowed(&device, &args)?;
    let label = arg_text(&args, "label").unwrap_or_else(|| "manual".into());
    let capture = capture_device_screenshot(&device, &label)?;
    Ok(json!({
        "status": "captured",
        "device": device,
        "screenshot": capture
    }))
}

pub(crate) fn call_device_geo_fix(args: Value) -> Result<Value, String> {
    let device = selected_device(&args)?;
    ensure_emulator_allowed(&device, &args)?;
    let point = point_from_args(&args)?;
    run_geo_fix(&device, &point)?;
    Ok(json!({
        "status": "OK",
        "device": device,
        "point": point,
        "adbCommand": "adb -s <device> emu geo fix <lng> <lat>"
    }))
}

pub(crate) fn call_device_geo_route_plan(args: Value) -> Result<Value, String> {
    let plan = route_plan_from_args(&args)?;
    Ok(json!({
        "status": "planned",
        "speedKmh": plan.speed_kmh,
        "durationSeconds": plan.duration_seconds,
        "tickSeconds": plan.tick_seconds,
        "tickCount": plan.points.len(),
        "routeDistanceMeters": round2(plan.route_distance_m),
        "simulatedDistanceMeters": round2(plan.simulated_distance_m),
        "distancePerTickMeters": round2(plan.distance_per_tick_m),
        "reachesRouteEnd": plan.reaches_route_end,
        "points": plan.points,
    }))
}

pub(crate) fn call_device_geo_route_start(args: Value) -> Result<Value, String> {
    let device = selected_device(&args)?;
    ensure_emulator_allowed(&device, &args)?;
    let plan = route_plan_from_args(&args)?;
    let screen_monitor = screen_monitor_config_from_args(&args);
    let route_id = arg_text(&args, "routeId")
        .unwrap_or_else(|| format!("GEO-{}", Utc::now().format("%Y%m%d%H%M%S%3f")));
    let now = Utc::now().to_rfc3339();
    let state = Arc::new(Mutex::new(RouteRunState {
        route_id: route_id.clone(),
        device: device.clone(),
        status: "RUNNING".into(),
        started_at: now.clone(),
        updated_at: now,
        speed_kmh: plan.speed_kmh,
        duration_seconds: plan.duration_seconds,
        tick_seconds: plan.tick_seconds,
        total_points: plan.points.len(),
        current_index: 0,
        last_point: None,
        last_error: None,
        screen_monitor_enabled: screen_monitor.enabled,
        screen_sample_every_ticks: screen_monitor.sample_every_ticks,
        screen_require_change: screen_monitor.require_change,
        screen_samples: 0,
        screen_stale_ticks: 0,
        screen_max_stale_ticks: screen_monitor.max_stale_ticks,
        last_screenshot_path: None,
        last_screenshot_sha256: None,
        last_screen_changed: None,
        screen_evidence: Vec::new(),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut guard = routes().lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            route_id.clone(),
            RouteRun {
                state: Arc::clone(&state),
                stop: Arc::clone(&stop),
            },
        );
    }

    let response_device = device.clone();
    let response_duration_seconds = plan.duration_seconds;
    let response_tick_seconds = plan.tick_seconds;
    let response_tick_count = plan.points.len();
    let response_speed_kmh = plan.speed_kmh;
    let thread_device = device.clone();
    let thread_points = plan.points.clone();
    let thread_tick_seconds = plan.tick_seconds;
    let thread_route_id = route_id.clone();
    thread::spawn(move || {
        let mut previous_screen_sha: Option<String> = None;
        let mut stale_screen_ticks = 0usize;
        for (idx, point) in thread_points.iter().enumerate() {
            if stop.load(Ordering::SeqCst) {
                update_state(&state, "STOPPED", idx, Some(point.clone()), None);
                return;
            }
            match run_geo_fix(&thread_device, point) {
                Ok(_) => update_state(&state, "RUNNING", idx + 1, Some(point.clone()), None),
                Err(e) => {
                    update_state(&state, "FAILED", idx, Some(point.clone()), Some(e));
                    return;
                }
            }
            if screen_monitor.enabled && idx % screen_monitor.sample_every_ticks == 0 {
                let label = format!("{}_tick_{}", thread_route_id, idx + 1);
                match capture_device_screenshot(&thread_device, &label) {
                    Ok(capture) => {
                        let changed = previous_screen_sha
                            .as_ref()
                            .map(|previous| previous != &capture.sha256);
                        if changed == Some(false) {
                            stale_screen_ticks += 1;
                        } else {
                            stale_screen_ticks = 0;
                        }
                        update_screen_state(
                            &state,
                            idx + 1,
                            capture.clone(),
                            changed,
                            stale_screen_ticks,
                        );
                        previous_screen_sha = Some(capture.sha256);
                        if screen_monitor.require_change
                            && stale_screen_ticks >= screen_monitor.max_stale_ticks
                        {
                            update_state(
                                &state,
                                "FAILED",
                                idx + 1,
                                Some(point.clone()),
                                Some(format!(
                                    "screen monitor stale for {stale_screen_ticks} samples"
                                )),
                            );
                            return;
                        }
                    }
                    Err(e) => {
                        update_state(
                            &state,
                            "FAILED",
                            idx + 1,
                            Some(point.clone()),
                            Some(format!("screen capture failed: {e}")),
                        );
                        return;
                    }
                }
            }
            thread::sleep(Duration::from_secs(thread_tick_seconds));
        }
        let last = thread_points.last().cloned();
        update_state(&state, "COMPLETED", thread_points.len(), last, None);
    });

    Ok(json!({
        "status": "started",
        "routeId": route_id,
        "device": response_device,
        "durationSeconds": response_duration_seconds,
        "tickSeconds": response_tick_seconds,
        "tickCount": response_tick_count,
        "speedKmh": response_speed_kmh,
        "screenMonitor": {
            "enabled": screen_monitor.enabled,
            "sampleEveryTicks": screen_monitor.sample_every_ticks,
            "requireChange": screen_monitor.require_change,
            "maxStaleTicks": screen_monitor.max_stale_ticks
        },
        "safety": "emulator-only unless allowPhysicalDevice=true"
    }))
}

pub(crate) fn call_device_geo_roam_plan(args: Value) -> Result<Value, String> {
    let roam = roam_points_from_args(&args)?;
    let mut route_args = args.clone();
    route_args["points"] = json!(roam.points);
    let plan = route_plan_from_args(&route_args)?;
    Ok(json!({
        "status": "planned",
        "mode": "city_roam_road",
        "city": roam.city,
        "center": roam.center,
        "radiusMeters": roam.radius_m,
        "avoidRepeat": true,
        "roadProvider": roam.road_provider,
        "roadRouted": roam.road_routed,
        "speedKmh": plan.speed_kmh,
        "durationSeconds": plan.duration_seconds,
        "tickSeconds": plan.tick_seconds,
        "tickCount": plan.points.len(),
        "routeDistanceMeters": round2(plan.route_distance_m),
        "simulatedDistanceMeters": round2(plan.simulated_distance_m),
        "distancePerTickMeters": round2(plan.distance_per_tick_m),
        "seedWaypoints": roam.seed_waypoints,
        "roadRoutePoints": route_args["points"].clone(),
        "points": plan.points
    }))
}

pub(crate) fn call_device_geo_roam_start(args: Value) -> Result<Value, String> {
    let roam = roam_points_from_args(&args)?;
    let mut route_args = args.clone();
    route_args["points"] = json!(roam.points);
    let started = call_device_geo_route_start(route_args)?;
    Ok(json!({
        "status": started.get("status").cloned().unwrap_or_else(|| json!("started")),
        "mode": "city_roam_road",
        "city": roam.city,
        "center": roam.center,
        "radiusMeters": roam.radius_m,
        "roadProvider": roam.road_provider,
        "roadRouted": roam.road_routed,
        "avoidRepeat": true,
        "route": started
    }))
}

pub(crate) fn call_device_geo_route_status(args: Value) -> Result<Value, String> {
    let route_id = arg_text(&args, "routeId").or_else(|| arg_text(&args, "route_id"));
    let guard = routes().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = route_id {
        let run = guard
            .get(&id)
            .ok_or_else(|| format!("route not found: {id}"))?;
        return Ok(json!({ "route": snapshot(&run.state) }));
    }
    let items = guard
        .values()
        .map(|run| snapshot(&run.state))
        .collect::<Vec<_>>();
    Ok(json!({ "routes": items }))
}

pub(crate) fn call_device_geo_route_stop(args: Value) -> Result<Value, String> {
    let route_id = arg_text(&args, "routeId")
        .or_else(|| arg_text(&args, "route_id"))
        .ok_or_else(|| "routeId is required".to_string())?;
    let guard = routes().lock().unwrap_or_else(|e| e.into_inner());
    let run = guard
        .get(&route_id)
        .ok_or_else(|| format!("route not found: {route_id}"))?;
    run.stop.store(true, Ordering::SeqCst);
    Ok(json!({
        "status": "stop_requested",
        "routeId": route_id,
        "route": snapshot(&run.state)
    }))
}

struct RoutePlan {
    speed_kmh: f64,
    duration_seconds: u64,
    tick_seconds: u64,
    route_distance_m: f64,
    simulated_distance_m: f64,
    distance_per_tick_m: f64,
    reaches_route_end: bool,
    points: Vec<GeoPoint>,
}

struct RoamSeed {
    city: String,
    center: GeoPoint,
    radius_m: f64,
    road_provider: String,
    road_routed: bool,
    seed_waypoints: Vec<GeoPoint>,
    points: Vec<GeoPoint>,
}

fn roam_points_from_args(args: &Value) -> Result<RoamSeed, String> {
    let center = if let Some(center) = args.get("center") {
        point_from_value(center)?
    } else if let Some(city) = arg_text(args, "city") {
        city_center(&city).ok_or_else(|| format!("unknown city: {city}; pass center instead"))?
    } else {
        city_center("taipei").expect("default city exists")
    };
    let city = arg_text(args, "city").unwrap_or_else(|| "taipei".into());
    let radius_m = f64_param(args, "radiusMeters", DEFAULT_ROAM_RADIUS_M)
        .or_else(|| f64_param(args, "radius_meters", DEFAULT_ROAM_RADIUS_M))
        .unwrap_or(DEFAULT_ROAM_RADIUS_M)
        .clamp(100.0, 10_000.0);
    let waypoint_count = u64_param(args, "waypoints", 8).unwrap_or(8).clamp(4, 32) as usize;
    let mut seed_waypoints = Vec::with_capacity(waypoint_count + 1);
    seed_waypoints.push(center.clone());
    // Deterministic golden-angle sweep. It avoids immediate backtracking and
    // keeps route targets distinct before the road router snaps them to roads.
    let golden_angle = 137.507764_f64.to_radians();
    for idx in 0..waypoint_count {
        let angle = idx as f64 * golden_angle;
        let scale = 0.45 + 0.5 * ((idx % 5) as f64 / 4.0);
        let distance_m = radius_m * scale;
        seed_waypoints.push(offset_point(&center, distance_m, angle));
    }
    let road_provider = arg_text(args, "roadProvider")
        .or_else(|| arg_text(args, "road_provider"))
        .or_else(|| std::env::var("SIRIN_ROAD_PROVIDER").ok())
        .unwrap_or_else(|| DEFAULT_ROAD_PROVIDER.into())
        .to_ascii_lowercase();
    let allow_synthetic_fallback = bool_param(args, "allowSyntheticFallback", false)
        || bool_param(args, "allow_synthetic_fallback", false);
    let (points, road_routed) = if road_provider == "synthetic" || road_provider == "none" {
        (seed_waypoints.clone(), false)
    } else {
        match road_route_points(&seed_waypoints, args) {
            Ok(points) => (points, true),
            Err(e) if allow_synthetic_fallback => {
                tracing::warn!(
                    "[device_geo] road routing failed, falling back to synthetic route: {e}"
                );
                (seed_waypoints.clone(), false)
            }
            Err(e) => return Err(e),
        }
    };
    Ok(RoamSeed {
        city,
        center,
        radius_m,
        road_provider,
        road_routed,
        seed_waypoints,
        points,
    })
}

fn road_route_points(seed_waypoints: &[GeoPoint], args: &Value) -> Result<Vec<GeoPoint>, String> {
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
        .header("User-Agent", "Sirin device_geo road-router")
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
            Ok(GeoPoint { lat, lng })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if points.len() < 2 {
        return Err("road router returned fewer than two route points".into());
    }
    Ok(points)
}

fn city_center(city: &str) -> Option<GeoPoint> {
    match city.trim().to_ascii_lowercase().as_str() {
        "taipei" | "台北" | "臺北" => Some(GeoPoint {
            lat: 25.033964,
            lng: 121.564468,
        }),
        "new_taipei" | "new taipei" | "新北" => Some(GeoPoint {
            lat: 25.011002,
            lng: 121.465661,
        }),
        "taichung" | "台中" | "臺中" => Some(GeoPoint {
            lat: 24.147736,
            lng: 120.673648,
        }),
        "kaohsiung" | "高雄" => Some(GeoPoint {
            lat: 22.627278,
            lng: 120.301435,
        }),
        _ => None,
    }
}

fn route_plan_from_args(args: &Value) -> Result<RoutePlan, String> {
    let route = route_points_from_args(args)?;
    if route.len() < 2 {
        return Err("route requires at least two points".into());
    }
    let speed_kmh = f64_param(args, "speedKmh", DEFAULT_SPEED_KMH)
        .or_else(|| f64_param(args, "speed_kmh", DEFAULT_SPEED_KMH))
        .unwrap_or(DEFAULT_SPEED_KMH);
    if speed_kmh <= 0.0 {
        return Err("speedKmh must be > 0".into());
    }
    let duration_seconds = u64_param(args, "durationSeconds", DEFAULT_DURATION_SECONDS)
        .or_else(|| u64_param(args, "duration_seconds", DEFAULT_DURATION_SECONDS))
        .unwrap_or(DEFAULT_DURATION_SECONDS)
        .clamp(1, 86_400);
    let tick_seconds = u64_param(args, "tickSeconds", DEFAULT_TICK_SECONDS)
        .or_else(|| u64_param(args, "tick_seconds", DEFAULT_TICK_SECONDS))
        .unwrap_or(DEFAULT_TICK_SECONDS)
        .clamp(1, 60);
    let speed_mps = speed_kmh * 1000.0 / 3600.0;
    let distance_per_tick_m = speed_mps * tick_seconds as f64;
    let route_distance_m = polyline_distance_m(&route);
    let simulated_distance_m = speed_mps * duration_seconds as f64;
    let tick_count = (duration_seconds / tick_seconds).max(1) as usize;
    let mut points = Vec::with_capacity(tick_count + 1);
    for i in 0..=tick_count {
        let target_m = (i as f64 * distance_per_tick_m).min(route_distance_m);
        points.push(interpolate_polyline(&route, target_m));
    }
    Ok(RoutePlan {
        speed_kmh,
        duration_seconds,
        tick_seconds,
        route_distance_m,
        simulated_distance_m,
        distance_per_tick_m,
        reaches_route_end: simulated_distance_m >= route_distance_m,
        points,
    })
}

fn route_points_from_args(args: &Value) -> Result<Vec<GeoPoint>, String> {
    if let Some(points) = args.get("points").and_then(Value::as_array) {
        return points.iter().map(point_from_value).collect();
    }
    if let Some(route) = args.get("route") {
        if let Some(points) = route.get("points").and_then(Value::as_array) {
            return points.iter().map(point_from_value).collect();
        }
        let start = route.get("start").ok_or("route.start is required")?;
        let end = route.get("end").ok_or("route.end is required")?;
        return Ok(vec![point_from_value(start)?, point_from_value(end)?]);
    }
    let start = args.get("start").ok_or("start or route is required")?;
    let end = args.get("end").ok_or("end is required")?;
    Ok(vec![point_from_value(start)?, point_from_value(end)?])
}

fn point_from_args(args: &Value) -> Result<GeoPoint, String> {
    if let Some(point) = args.get("point") {
        return point_from_value(point);
    }
    point_from_value(args)
}

fn point_from_value(value: &Value) -> Result<GeoPoint, String> {
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
    Ok(GeoPoint { lat, lng })
}

fn selected_device(args: &Value) -> Result<String, String> {
    if let Some(device) = arg_text(args, "device").or_else(|| arg_text(args, "serial")) {
        return Ok(device);
    }
    let adb = adb_path()?;
    let devices = adb_devices(&adb)?;
    devices
        .iter()
        .filter_map(|d| d.get("serial").and_then(Value::as_str))
        .find(|serial| serial.starts_with("emulator-"))
        .map(str::to_string)
        .ok_or_else(|| "no emulator device found; start an Android Emulator first".to_string())
}

fn ensure_emulator_allowed(device: &str, args: &Value) -> Result<(), String> {
    if device.starts_with("emulator-") {
        return Ok(());
    }
    if bool_param(args, "allowPhysicalDevice", false)
        || bool_param(args, "allow_physical_device", false)
    {
        return Ok(());
    }
    Err(format!(
        "refusing to control physical/non-emulator device {device}; pass allowPhysicalDevice=true explicitly"
    ))
}

fn run_geo_fix(device: &str, point: &GeoPoint) -> Result<(), String> {
    let adb = adb_path()?;
    let output = Command::new(adb)
        .no_window()
        .args([
            "-s",
            device,
            "emu",
            "geo",
            "fix",
            &format!("{:.7}", point.lng),
            &format!("{:.7}", point.lat),
        ])
        .output()
        .map_err(|e| format!("adb geo fix failed to start: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "adb geo fix failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn capture_device_screenshot(device: &str, label: &str) -> Result<ScreenshotCapture, String> {
    let adb = adb_path()?;
    let output = Command::new(adb)
        .no_window()
        .args(["-s", device, "exec-out", "screencap", "-p"])
        .output()
        .map_err(|e| format!("adb screencap failed to start: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "adb screencap failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if output.stdout.is_empty() {
        return Err("adb screencap returned empty PNG".into());
    }

    let dir = crate::platform::app_data_dir().join("device_screenshots");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create screenshot dir {}: {e}", dir.display()))?;
    let file_name = format!(
        "{}_{}.png",
        Utc::now().format("%Y%m%d%H%M%S%3f"),
        sanitize_file_part(label)
    );
    let path = dir.join(file_name);
    fs::write(&path, &output.stdout)
        .map_err(|e| format!("failed to write screenshot {}: {e}", path.display()))?;
    Ok(ScreenshotCapture {
        path: path.display().to_string(),
        size_bytes: output.stdout.len() as u64,
        sha256: sha256_hex(&output.stdout),
    })
}

fn adb_devices(adb: &PathBuf) -> Result<Vec<Value>, String> {
    let output = Command::new(adb)
        .no_window()
        .args(["devices", "-l"])
        .output()
        .map_err(|e| format!("adb devices failed: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .skip(1)
        .filter_map(parse_adb_device_line)
        .collect())
}

fn parse_adb_device_line(line: &str) -> Option<Value> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.split_whitespace();
    let serial = parts.next()?;
    let state = parts.next().unwrap_or("unknown");
    Some(json!({
        "serial": serial,
        "state": state,
        "raw": line,
        "isEmulator": serial.starts_with("emulator-")
    }))
}

fn emulator_list_avds() -> Result<Vec<String>, String> {
    let emulator = emulator_path()?;
    if !emulator.exists() {
        return Ok(Vec::new());
    }
    let output = Command::new(emulator)
        .no_window()
        .arg("-list-avds")
        .output()
        .map_err(|e| format!("emulator -list-avds failed: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn first_ready_emulator(adb: &PathBuf) -> Result<Option<String>, String> {
    Ok(adb_devices(adb)?
        .iter()
        .filter(|d| d.get("state").and_then(Value::as_str) == Some("device"))
        .filter_map(|d| d.get("serial").and_then(Value::as_str))
        .find(|serial| serial.starts_with("emulator-"))
        .map(str::to_string))
}

fn boot_completed(adb: &PathBuf, device: &str) -> Result<bool, String> {
    let output = Command::new(adb)
        .no_window()
        .args(["-s", device, "shell", "getprop", "sys.boot_completed"])
        .output()
        .map_err(|e| format!("adb getprop failed: {e}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).trim() == "1")
}

fn foreground_package(adb: &PathBuf, device: &str) -> Result<Option<String>, String> {
    let output = Command::new(adb)
        .no_window()
        .args(["-s", device, "shell", "dumpsys", "window", "windows"])
        .output()
        .map_err(|e| format!("adb dumpsys failed: {e}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(idx) = line.find("mCurrentFocus=") {
            let text = &line[idx..];
            if let Some(pkg) = text.split_whitespace().find_map(|part| {
                let slash = part.find('/')?;
                Some(
                    part[..slash]
                        .trim_matches(|c| c == '{' || c == '}')
                        .to_string(),
                )
            }) {
                if !pkg.is_empty() {
                    return Ok(Some(pkg));
                }
            }
        }
    }
    Ok(None)
}

fn emulator_path() -> Result<PathBuf, String> {
    let sdk = android_sdk_root().ok_or_else(|| "Android SDK root not found".to_string())?;
    let emulator = sdk.join("emulator").join(if cfg!(windows) {
        "emulator.exe"
    } else {
        "emulator"
    });
    if emulator.exists() {
        Ok(emulator)
    } else {
        Err(format!("emulator not found at {}", emulator.display()))
    }
}

fn adb_path() -> Result<PathBuf, String> {
    let sdk =
        android_sdk_root().ok_or_else(|| "ANDROID_SDK_ROOT/ANDROID_HOME not found".to_string())?;
    let path = sdk
        .join("platform-tools")
        .join(if cfg!(windows) { "adb.exe" } else { "adb" });
    if path.exists() {
        Ok(path)
    } else {
        Err(format!("adb not found at {}", path.display()))
    }
}

fn android_sdk_root() -> Option<PathBuf> {
    for key in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Ok(value) = std::env::var(key) {
            let path = PathBuf::from(value);
            if path.exists() {
                return Some(path);
            }
        }
    }
    for candidate in [PathBuf::from(r"D:\Android\Sdk"), dirs_local_android_sdk()] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn android_avd_home() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("ANDROID_AVD_HOME") {
        let path = PathBuf::from(value);
        if path.exists() {
            return Some(path);
        }
    }
    let d = PathBuf::from(r"D:\Android\Avd");
    if d.exists() {
        return Some(d);
    }
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .ok()
        .map(|p| p.join(".android").join("avd"))
        .filter(|p| p.exists())
}

fn dirs_local_android_sdk() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(""))
        .join("Android")
        .join("Sdk")
}

fn update_state(
    state: &Arc<Mutex<RouteRunState>>,
    status: &str,
    current_index: usize,
    last_point: Option<GeoPoint>,
    last_error: Option<String>,
) {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    guard.status = status.into();
    guard.updated_at = Utc::now().to_rfc3339();
    guard.current_index = current_index;
    guard.last_point = last_point;
    guard.last_error = last_error;
}

fn update_screen_state(
    state: &Arc<Mutex<RouteRunState>>,
    tick: usize,
    capture: ScreenshotCapture,
    changed: Option<bool>,
    stale_ticks: usize,
) {
    let mut guard = state.lock().unwrap_or_else(|e| e.into_inner());
    guard.updated_at = Utc::now().to_rfc3339();
    guard.screen_samples += 1;
    guard.screen_stale_ticks = stale_ticks;
    guard.last_screenshot_path = Some(capture.path.clone());
    guard.last_screenshot_sha256 = Some(capture.sha256.clone());
    guard.last_screen_changed = changed;
    guard.screen_evidence.push(ScreenEvidence {
        tick,
        captured_at: Utc::now().to_rfc3339(),
        path: capture.path,
        size_bytes: capture.size_bytes,
        sha256: capture.sha256,
        changed,
    });
    if guard.screen_evidence.len() > 20 {
        let overflow = guard.screen_evidence.len() - 20;
        guard.screen_evidence.drain(0..overflow);
    }
}

fn snapshot(state: &Arc<Mutex<RouteRunState>>) -> Value {
    let guard = state.lock().unwrap_or_else(|e| e.into_inner());
    serde_json::to_value(&*guard).unwrap_or_else(|_| json!({}))
}

fn polyline_distance_m(points: &[GeoPoint]) -> f64 {
    points
        .windows(2)
        .map(|pair| haversine_m(&pair[0], &pair[1]))
        .sum()
}

fn interpolate_polyline(points: &[GeoPoint], target_m: f64) -> GeoPoint {
    let mut remaining = target_m;
    for pair in points.windows(2) {
        let a = &pair[0];
        let b = &pair[1];
        let segment = haversine_m(a, b);
        if remaining <= segment || segment == 0.0 {
            let ratio = if segment == 0.0 {
                0.0
            } else {
                remaining / segment
            };
            return GeoPoint {
                lat: a.lat + (b.lat - a.lat) * ratio,
                lng: a.lng + (b.lng - a.lng) * ratio,
            };
        }
        remaining -= segment;
    }
    points
        .last()
        .cloned()
        .unwrap_or(GeoPoint { lat: 0.0, lng: 0.0 })
}

fn haversine_m(a: &GeoPoint, b: &GeoPoint) -> f64 {
    let lat1 = a.lat.to_radians();
    let lat2 = b.lat.to_radians();
    let dlat = (b.lat - a.lat).to_radians();
    let dlng = (b.lng - a.lng).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlng / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

fn offset_point(center: &GeoPoint, distance_m: f64, bearing_rad: f64) -> GeoPoint {
    let angular = distance_m / EARTH_RADIUS_M;
    let lat1 = center.lat.to_radians();
    let lon1 = center.lng.to_radians();
    let lat2 = (lat1.sin() * angular.cos() + lat1.cos() * angular.sin() * bearing_rad.cos()).asin();
    let lon2 = lon1
        + (bearing_rad.sin() * angular.sin() * lat1.cos())
            .atan2(angular.cos() - lat1.sin() * lat2.sin());
    GeoPoint {
        lat: lat2.to_degrees(),
        lng: lon2.to_degrees(),
    }
}

fn arg_text(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn f64_param(args: &Value, key: &str, _default: f64) -> Option<f64> {
    args.get(key).and_then(Value::as_f64)
}

fn u64_param(args: &Value, key: &str, _default: u64) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn bool_param(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn screen_monitor_config_from_args(args: &Value) -> ScreenMonitorConfig {
    let enabled =
        bool_param(args, "screenMonitor", false) || bool_param(args, "screen_monitor", false);
    let sample_every_ticks = u64_param(args, "screenSampleEveryTicks", 5)
        .or_else(|| u64_param(args, "screen_sample_every_ticks", 5))
        .unwrap_or(5)
        .clamp(1, 600) as usize;
    let require_change = bool_param(args, "screenRequireChange", false)
        || bool_param(args, "screen_require_change", false);
    let max_stale_ticks = u64_param(args, "maxStaleScreenTicks", 3)
        .or_else(|| u64_param(args, "max_stale_screen_ticks", 3))
        .unwrap_or(3)
        .clamp(1, 100) as usize;
    ScreenMonitorConfig {
        enabled,
        sample_every_ticks,
        require_change,
        max_stale_ticks,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
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
        "screenshot".into()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_plan_20kmh_for_10min_is_about_3333m() {
        let plan = route_plan_from_args(&json!({
            "route": {
                "start": {"lat": 25.033964, "lng": 121.564468},
                "end": {"lat": 25.064000, "lng": 121.564468}
            },
            "speedKmh": 20.0,
            "durationSeconds": 600,
            "tickSeconds": 1
        }))
        .expect("plan");

        assert_eq!(plan.points.len(), 601);
        assert!((plan.simulated_distance_m - 3333.33).abs() < 1.0);
        assert!((plan.distance_per_tick_m - 5.55).abs() < 0.1);
    }

    #[test]
    fn physical_device_requires_explicit_override() {
        assert!(ensure_emulator_allowed("emulator-5554", &json!({})).is_ok());
        assert!(ensure_emulator_allowed("R58N123ABC", &json!({})).is_err());
        assert!(
            ensure_emulator_allowed("R58N123ABC", &json!({"allowPhysicalDevice": true})).is_ok()
        );
    }

    #[test]
    fn roam_plan_generates_non_repeated_waypoints_inside_radius() {
        let roam = roam_points_from_args(&json!({
            "city": "taipei",
            "radiusMeters": 1500.0,
            "waypoints": 8,
            "roadProvider": "synthetic"
        }))
        .expect("roam");

        assert_eq!(roam.points.len(), 9);
        for point in roam.points.iter().skip(1) {
            assert!(haversine_m(&roam.center, point) <= 1500.1);
        }
        let unique = roam
            .points
            .iter()
            .map(|p| format!("{:.5},{:.5}", p.lat, p.lng))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), roam.points.len());
    }

    #[test]
    fn roam_plan_defaults_to_20kmh_10min() {
        let value = call_device_geo_roam_plan(json!({
            "city": "taipei",
            "radiusMeters": 1500.0,
            "roadProvider": "synthetic"
        }))
        .expect("plan");

        assert_eq!(value["speedKmh"], 20.0);
        assert_eq!(value["durationSeconds"], 600);
        assert_eq!(value["tickSeconds"], 1);
        assert_eq!(value["tickCount"], 601);
        assert_eq!(value["roadRouted"], false);
    }

    #[test]
    fn roam_defaults_to_osrm_road_provider() {
        assert_eq!(
            arg_text(&json!({}), "roadProvider")
                .or_else(|| std::env::var("SIRIN_ROAD_PROVIDER").ok())
                .unwrap_or_else(|| DEFAULT_ROAD_PROVIDER.into()),
            "osrm"
        );
    }

    #[test]
    fn screen_monitor_defaults_to_opt_in() {
        let config = screen_monitor_config_from_args(&json!({}));
        assert!(!config.enabled);
        assert_eq!(config.sample_every_ticks, 5);
        assert!(!config.require_change);
        assert_eq!(config.max_stale_ticks, 3);

        let enabled = screen_monitor_config_from_args(&json!({
            "screenMonitor": true,
            "screenSampleEveryTicks": 1,
            "screenRequireChange": true,
            "maxStaleScreenTicks": 4
        }));
        assert!(enabled.enabled);
        assert_eq!(enabled.sample_every_ticks, 1);
        assert!(enabled.require_change);
        assert_eq!(enabled.max_stale_ticks, 4);
    }

    #[test]
    fn screenshot_hash_is_stable_sha256() {
        assert_eq!(
            sha256_hex(b"sirin"),
            "1dc3c45da5c876a82e5b7232822da29c2b67fbb3009d308431f56b5f3007c949"
        );
        assert_eq!(sanitize_file_part("GEO:1/tick 2"), "GEO_1_tick_2");
    }
}
