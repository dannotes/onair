//! Axum web API + dashboard server.
//!
//! Spawned by `main` alongside the monitor and bulb-poll loops. Exposes:
//! - `GET  /`                  — embedded HTML dashboard
//! - `GET  /api/status`        — full live state snapshot (polled every 3 s)
//! - `GET  /api/logs`          — paginated event ring buffer
//! - `GET  /api/config`        — current config
//! - `POST /api/config`        — partial config update
//! - `POST /api/override`      — set override mode
//! - `POST /api/discover`      — trigger LAN bulb discovery
//! - `POST /api/bulb/select`   — pick a bulb by MAC
//! - `GET  /api/bulb/state`    — cached `getPilot` snapshot
//! - `POST /api/bulb/test`     — flash the bulb in call/idle for 3 s
//! - `POST /api/teams/verify`  — verify a Teams log directory
//! - `GET  /api/calls`         — call history (stub until SQLite — chunk 6)
//!
//! Critical invariant: never hold a `parking_lot` guard across an `.await`.
//! Read into a local first, drop the guard, then await.

use crate::autostart;
use crate::bulb::{self, BulbState, DiscoveredBulb};
use crate::models::Rgb;
use crate::platform;
use crate::presence::LogWatcher;
use crate::state::{
    self, AppState, BulbMode, Config, DisplayState, Event, EventLevel, OverrideMode,
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;

// ============================================================================
// Server entry point
// ============================================================================

/// Spawn the Axum server. Binds to `127.0.0.1:{ui_port}` from current config.
pub async fn serve(state: Arc<AppState>) -> anyhow::Result<()> {
    let port = state.config.read().ui_port;

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/logs", get(logs))
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/override", post(post_override))
        .route("/api/discover", post(post_discover))
        .route("/api/bulb/select", post(post_bulb_select))
        .route("/api/bulb/probe", post(post_bulb_probe))
        .route("/api/bulb/state", get(get_bulb_state))
        .route("/api/bulb/test", post(post_bulb_test))
        .route("/api/teams/verify", post(post_teams_verify))
        .route("/api/calls", get(get_calls))
        .route("/api/autostart", get(get_autostart).post(post_autostart))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("127.0.0.1:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("web server listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

// ============================================================================
// Index — embedded dashboard
// ============================================================================

const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

// ============================================================================
// Helpers
// ============================================================================

/// Convert an i32 RSSI in dBm to a coarse signal-quality bucket.
fn signal_quality(rssi: i32) -> &'static str {
    if rssi > -50 {
        "Excellent"
    } else if rssi >= -70 {
        "Good"
    } else if rssi >= -85 {
        "Fair"
    } else {
        "Poor"
    }
}

/// JSON error helper — returns (status, body).
fn err_json(status: StatusCode, msg: impl Into<String>) -> Response {
    let body = serde_json::json!({ "ok": false, "error": msg.into() });
    (status, Json(body)).into_response()
}

// ============================================================================
// /api/status
// ============================================================================

#[derive(Serialize)]
struct BulbLiveDto {
    state: bool,
    r: u8,
    g: u8,
    b: u8,
    dimming: u8,
    temp: u32,
    rssi: i32,
    scene_id: u32,
    signal_quality: &'static str,
    last_polled: Option<DateTime<Local>>,
}

#[derive(Serialize)]
struct StatusDto {
    version: &'static str,
    status: String,
    display: DisplayState,
    bulb_mac: String,
    bulb_ip: Option<String>,
    bulb_connected: bool,
    bulb_live: Option<BulbLiveDto>,
    call_start: Option<DateTime<Local>>,
    call_duration_secs: i64,
    calls_today: u32,
    total_time_today_secs: u64,
    uptime_secs: i64,
    work_hours_active: bool,
    #[serde(rename = "override")]
    override_mode: OverrideMode,
    grace_pending: bool,
    grace_remaining_secs: Option<u64>,
}

async fn status(State(state): State<Arc<AppState>>) -> Json<StatusDto> {
    // Snapshot everything into locals so no parking_lot guards are held while we
    // build the response.
    let presence = *state.current_presence.read();
    let display = *state.display.read();
    let bulb_mac = state.config.read().bulb_mac.clone();
    let bulb_ip_opt: Option<Ipv4Addr> = *state.bulb_ip.read();
    let bulb_connected = *state.bulb_reachable.read();
    let bulb_live_opt: Option<BulbState> = state.bulb_live.read().clone();
    let bulb_last_polled: Option<DateTime<Local>> = *state.bulb_last_polled.read();
    let call_start_opt: Option<DateTime<Local>> = *state.call_start.read();
    let stats = state.stats.read().clone();
    let override_mode = *state.override_mode.read();
    let grace_until_opt = *state.grace_until.read();
    let grace_pending = grace_until_opt.is_some();
    let grace_remaining_secs = grace_until_opt.map(|t| {
        t.saturating_duration_since(std::time::Instant::now())
            .as_secs()
    });
    let work_hours_active = state.within_work_hours();

    let bulb_live = if bulb_connected {
        bulb_live_opt.map(|b| BulbLiveDto {
            state: b.state,
            r: b.r,
            g: b.g,
            b: b.b,
            dimming: b.dimming,
            temp: b.temp,
            rssi: b.rssi,
            scene_id: b.scene_id,
            signal_quality: signal_quality(b.rssi),
            last_polled: bulb_last_polled,
        })
    } else {
        None
    };

    let call_duration_secs = call_start_opt
        .map(|s| (Local::now() - s).num_seconds().max(0))
        .unwrap_or(0);

    let uptime_secs = (Local::now() - state.started_at).num_seconds().max(0);

    let status_str = match presence {
        crate::models::Presence::Available => "available",
        crate::models::Presence::Busy => "busy",
        crate::models::Presence::Away => "away",
        crate::models::Presence::BeRightBack => "berightback",
        crate::models::Presence::DoNotDisturb => "donotdisturb",
        crate::models::Presence::Offline => "offline",
        crate::models::Presence::Unknown => "unknown",
    }
    .to_string();

    Json(StatusDto {
        version: env!("CARGO_PKG_VERSION"),
        status: status_str,
        display,
        bulb_mac,
        bulb_ip: bulb_ip_opt.map(|ip| ip.to_string()),
        bulb_connected,
        bulb_live,
        call_start: call_start_opt,
        call_duration_secs,
        calls_today: stats.calls_today,
        total_time_today_secs: stats.total_time_today_secs,
        uptime_secs,
        work_hours_active,
        override_mode,
        grace_pending,
        grace_remaining_secs,
    })
}

// ============================================================================
// /api/logs
// ============================================================================

#[derive(Deserialize)]
struct LogsQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Serialize)]
struct LogsResponse {
    events: Vec<Event>,
    total: usize,
}

async fn logs(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LogsQuery>,
) -> Json<LogsResponse> {
    let limit = q.limit.unwrap_or(100);
    let offset = q.offset.unwrap_or(0);
    let events = state.get_events(limit, offset);
    let total = state.total_events();
    Json(LogsResponse { events, total })
}

// ============================================================================
// /api/config
// ============================================================================

async fn get_config(State(state): State<Arc<AppState>>) -> Json<Config> {
    let cfg = state.config.read().clone();
    Json(cfg)
}

async fn post_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let obj = match body.as_object() {
        Some(o) => o,
        None => return err_json(StatusCode::BAD_REQUEST, "expected JSON object"),
    };

    // Helper closures capture `obj` for ergonomic key extraction.
    let get_u8 = |k: &str| -> Option<u8> { obj.get(k).and_then(|v| v.as_u64()).map(|n| n as u8) };
    let get_u64 = |k: &str| -> Option<u64> { obj.get(k).and_then(|v| v.as_u64()) };
    let get_u16 =
        |k: &str| -> Option<u16> { obj.get(k).and_then(|v| v.as_u64()).map(|n| n as u16) };
    let get_str =
        |k: &str| -> Option<String> { obj.get(k).and_then(|v| v.as_str()).map(|s| s.to_string()) };
    let get_color = |k: &str| -> Option<Result<Rgb, String>> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| Rgb::from_hex(s).ok_or_else(|| format!("invalid hex color for {}: {}", k, s)))
    };
    let get_bulb_mode = |k: &str| -> Option<Result<BulbMode, String>> {
        obj.get(k).and_then(|v| v.as_str()).map(|s| match s {
            "on" => Ok(BulbMode::On),
            "off" => Ok(BulbMode::Off),
            other => Err(format!("invalid bulb mode for {}: {}", k, other)),
        })
    };

    // Validate colors / modes up-front so we don't half-apply on failure.
    let call_color_parsed = match get_color("call_color") {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => return err_json(StatusCode::BAD_REQUEST, e),
        None => None,
    };
    let idle_color_parsed = match get_color("idle_color") {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => return err_json(StatusCode::BAD_REQUEST, e),
        None => None,
    };
    let call_state_parsed = match get_bulb_mode("call_state") {
        Some(Ok(m)) => Some(m),
        Some(Err(e)) => return err_json(StatusCode::BAD_REQUEST, e),
        None => None,
    };
    let idle_state_parsed = match get_bulb_mode("idle_state") {
        Some(Ok(m)) => Some(m),
        Some(Err(e)) => return err_json(StatusCode::BAD_REQUEST, e),
        None => None,
    };

    {
        let mut cfg = state.config.write();
        if let Some(v) = get_u8("work_start") {
            cfg.work_start = v;
        }
        if let Some(v) = get_u8("work_end") {
            cfg.work_end = v;
        }
        if let Some(v) = get_u64("poll_interval_secs") {
            cfg.poll_interval_secs = v;
        }
        if let Some(v) = get_u64("grace_period_secs") {
            cfg.grace_period_secs = v;
        }
        if let Some(v) = get_u64("max_call_hours") {
            cfg.max_call_hours = v;
        }
        if let Some(v) = get_u64("teams_offline_mins") {
            cfg.teams_offline_mins = v;
        }
        if let Some(m) = call_state_parsed {
            cfg.call_state = m;
        }
        if let Some(c) = call_color_parsed {
            cfg.call_color = c;
        }
        if let Some(v) = get_u8("call_brightness") {
            cfg.call_brightness = v;
        }
        if let Some(m) = idle_state_parsed {
            cfg.idle_state = m;
        }
        if let Some(c) = idle_color_parsed {
            cfg.idle_color = c;
        }
        if let Some(v) = get_u8("idle_brightness") {
            cfg.idle_brightness = v;
        }
        if let Some(v) = get_u16("ui_port") {
            cfg.ui_port = v;
        }
        if let Some(v) = get_str("log_level") {
            cfg.log_level = v;
        }
        if let Some(v) = get_str("bulb_mac") {
            cfg.bulb_mac = v;
        }
        // teams_log_dir: accept string (set), null (clear), or omitted (no change).
        if let Some(v) = obj.get("teams_log_dir") {
            if v.is_null() {
                cfg.teams_log_dir = None;
            } else if let Some(s) = v.as_str() {
                cfg.teams_log_dir = Some(PathBuf::from(s));
            }
        }
    }

    state.persist_config();
    state.log_event(EventLevel::Inf, "config updated");
    state::reconcile_display(&state).await;
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ============================================================================
// /api/override
// ============================================================================

#[derive(Deserialize)]
struct OverrideBody {
    mode: String,
}

async fn post_override(
    State(state): State<Arc<AppState>>,
    Json(body): Json<OverrideBody>,
) -> Response {
    let mode = match body.mode.as_str() {
        "auto" => OverrideMode::Auto,
        "force_red" => OverrideMode::ForceRed,
        "force_off" => OverrideMode::ForceOff,
        other => {
            return err_json(
                StatusCode::BAD_REQUEST,
                format!("unknown override mode: {}", other),
            );
        }
    };

    *state.override_mode.write() = mode;
    state.log_event(EventLevel::Inf, format!("override mode set to {:?}", mode));

    match mode {
        OverrideMode::ForceRed => state::force_red(&state).await,
        OverrideMode::ForceOff => state::force_off(&state).await,
        OverrideMode::Auto => {
            // Immediately re-evaluate so the bulb reflects what auto would do
            // instead of waiting for the next presence transition.
            state::reconcile_display(&state).await;
        }
    }

    Json(serde_json::json!({ "ok": true, "mode": body.mode })).into_response()
}

// ============================================================================
// /api/discover
// ============================================================================

#[derive(Serialize)]
struct DiscoverDto {
    mac: String,
    ip: String,
    module: Option<String>,
    selected: bool,
}

#[derive(Serialize)]
struct DiscoverResponse {
    bulbs: Vec<DiscoverDto>,
}

async fn post_discover(State(state): State<Arc<AppState>>) -> Response {
    let bulbs: Vec<DiscoveredBulb> = match bulb::discover(Duration::from_secs(3)).await {
        Ok(b) => b,
        Err(e) => {
            return err_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("discovery failed: {}", e),
            );
        }
    };

    *state.last_discovery.write() = bulbs.clone();
    let configured_mac = state.config.read().bulb_mac.clone();

    let dtos: Vec<DiscoverDto> = bulbs
        .into_iter()
        .map(|b| {
            let selected =
                !configured_mac.is_empty() && b.mac.eq_ignore_ascii_case(&configured_mac);
            DiscoverDto {
                mac: b.mac,
                ip: b.ip.to_string(),
                module: b.module,
                selected,
            }
        })
        .collect();

    Json(DiscoverResponse { bulbs: dtos }).into_response()
}

// ============================================================================
// /api/bulb/select
// ============================================================================

#[derive(Deserialize)]
struct BulbSelectBody {
    mac: String,
    /// Optional explicit IP. If provided, skip discovery lookup and bind
    /// directly. Useful when broadcast discovery is unreliable.
    #[serde(default)]
    ip: Option<String>,
}

async fn post_bulb_select(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BulbSelectBody>,
) -> Response {
    let mac = body.mac.trim().to_string();
    if mac.is_empty() {
        return err_json(StatusCode::BAD_REQUEST, "missing mac");
    }

    state.config.write().bulb_mac = mac.clone();
    state.persist_config();

    // Priority: (1) explicit IP from body, (2) last discovery, (3) fall back
    // to background re-discovery.
    let ip_opt: Option<Ipv4Addr> = body.ip.as_deref().and_then(|s| s.parse().ok()).or_else(|| {
        state
            .last_discovery
            .read()
            .iter()
            .find(|b| b.mac.eq_ignore_ascii_case(&mac))
            .map(|b| b.ip)
    });

    if let Some(ip) = ip_opt {
        *state.bulb_ip.write() = Some(ip);
        *state.bulb_reachable.write() = true;
        // Cache the IP so next startup skips broadcast discovery.
        state.config.write().bulb_last_ip = ip.to_string();
        state.persist_config();
        state.log_event(EventLevel::Ok, format!("bulb selected {} at {}", mac, ip));
        Json(serde_json::json!({
            "ok": true,
            "mac": mac,
            "ip": ip.to_string(),
        }))
        .into_response()
    } else {
        // Not in last discovery — kick off a fresh resolve in the background.
        state.log_event(
            EventLevel::Inf,
            format!(
                "bulb {} not in last discovery, resolving in background",
                mac
            ),
        );
        let s = state.clone();
        tokio::spawn(async move {
            state::resolve_bulb(s).await;
        });
        Json(serde_json::json!({
            "ok": true,
            "mac": mac,
            "ip": null,
        }))
        .into_response()
    }
}

// ============================================================================
// /api/bulb/probe
// ============================================================================

#[derive(Deserialize)]
struct BulbProbeBody {
    ip: String,
}

/// Manual escape hatch for "claimed bulb" WiZ behaviour: user types in
/// an IP they already know (from the router admin or the WiZ app), we
/// unicast `registration` to it, parse the MAC from the reply, and
/// persist both as the selected bulb. Bypasses broadcast discovery
/// entirely so it works even when the bulb refuses to respond to
/// broadcast registration requests.
async fn post_bulb_probe(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BulbProbeBody>,
) -> Response {
    let ip: Ipv4Addr = match body.ip.trim().parse() {
        Ok(ip) => ip,
        Err(_) => {
            return err_json(StatusCode::BAD_REQUEST, "invalid IPv4 address");
        }
    };

    let found = match bulb::probe(ip).await {
        Ok(b) => b,
        Err(e) => {
            state.log_event(
                EventLevel::Wrn,
                format!("manual probe {} failed: {}", ip, e),
            );
            return err_json(
                StatusCode::NOT_FOUND,
                format!(
                    "no bulb responded at {}: {} (check the IP in your router admin or the WiZ app)",
                    ip, e
                ),
            );
        }
    };

    // Persist the binding and go live immediately — no background resolve
    // step, since we already have a known-good unicast path.
    {
        let mut cfg = state.config.write();
        cfg.bulb_mac = found.mac.clone();
        cfg.bulb_last_ip = ip.to_string();
    }
    state.persist_config();
    *state.bulb_ip.write() = Some(ip);
    *state.bulb_reachable.write() = true;

    state.log_event(
        EventLevel::Ok,
        format!("manually selected bulb {} at {}", found.mac, ip),
    );

    Json(serde_json::json!({
        "ok": true,
        "mac": found.mac,
        "ip": ip.to_string(),
        "module": found.module,
    }))
    .into_response()
}

// ============================================================================
// /api/bulb/state
// ============================================================================

async fn get_bulb_state(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let reachable = *state.bulb_reachable.read();
    let live: Option<BulbState> = state.bulb_live.read().clone();
    let last_polled: Option<DateTime<Local>> = *state.bulb_last_polled.read();

    if !reachable || live.is_none() {
        return Json(serde_json::json!({ "connected": false }));
    }

    let b = live.unwrap();
    Json(serde_json::json!({
        "connected": true,
        "state": b.state,
        "r": b.r,
        "g": b.g,
        "b": b.b,
        "dimming": b.dimming,
        "temp": b.temp,
        "rssi": b.rssi,
        "scene_id": b.scene_id,
        "signal_quality": signal_quality(b.rssi),
        "last_polled": last_polled,
    }))
}

// ============================================================================
// /api/bulb/test
// ============================================================================

#[derive(Deserialize)]
struct BulbTestBody {
    mode: String,
}

async fn post_bulb_test(
    State(state): State<Arc<AppState>>,
    Json(body): Json<BulbTestBody>,
) -> Response {
    let ip_opt: Option<Ipv4Addr> = *state.bulb_ip.read();
    let Some(ip) = ip_opt else {
        return err_json(StatusCode::BAD_REQUEST, "no bulb selected");
    };

    // Snapshot current state so we can revert.
    let prev = match bulb::get_pilot(ip).await {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("test: get_pilot failed (continuing without revert): {}", e);
            None
        }
    };

    // Read the requested mode's config.
    let (mode, color, brightness, label) = {
        let cfg = state.config.read();
        match body.mode.as_str() {
            "call" => (cfg.call_state, cfg.call_color, cfg.call_brightness, "call"),
            "idle" => (cfg.idle_state, cfg.idle_color, cfg.idle_brightness, "idle"),
            other => {
                return err_json(
                    StatusCode::BAD_REQUEST,
                    format!("unknown test mode: {}", other),
                );
            }
        }
    };

    state.log_event(
        EventLevel::Inf,
        format!("bulb test: applying {} for 3s", label),
    );

    let apply_result = match mode {
        BulbMode::On => bulb::set_pilot_color(ip, color, brightness).await,
        BulbMode::Off => bulb::set_pilot_off(ip).await,
    };
    if let Err(e) = apply_result {
        return err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("bulb test failed: {}", e),
        );
    }

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Revert to previous state, if we captured one.
    if let Some(p) = prev {
        let revert = if p.state {
            bulb::set_pilot_color(ip, Rgb::new(p.r, p.g, p.b), p.dimming).await
        } else {
            bulb::set_pilot_off(ip).await
        };
        if let Err(e) = revert {
            tracing::warn!("test: revert failed: {}", e);
            state.log_event(EventLevel::Wrn, format!("bulb test revert failed: {}", e));
        }
    }

    Json(serde_json::json!({ "ok": true })).into_response()
}

// ============================================================================
// /api/teams/verify
// ============================================================================

#[derive(Deserialize)]
struct TeamsVerifyBody {
    #[serde(default)]
    path: Option<String>,
}

async fn post_teams_verify(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TeamsVerifyBody>,
) -> Response {
    let dir: Option<PathBuf> = body
        .path
        .map(PathBuf::from)
        .or_else(|| state.config.read().teams_log_dir.clone())
        .or_else(platform::default_teams_log_dir);

    let Some(dir) = dir else {
        return err_json(
            StatusCode::BAD_REQUEST,
            "no path provided and no default Teams log dir for this OS",
        );
    };

    let result = LogWatcher::verify(&dir);

    let latest_mtime_iso: Option<DateTime<Local>> =
        result.latest_mtime.map(DateTime::<Local>::from);

    Json(serde_json::json!({
        "dir_exists": result.dir_exists,
        "log_files_count": result.log_files_count,
        "latest_log": result.latest_log.as_ref().map(|p| p.display().to_string()),
        "latest_mtime": latest_mtime_iso,
        "sample_match": result.sample_match,
        "error": result.error,
    }))
    .into_response()
}

// ============================================================================
// /api/autostart
// ============================================================================

async fn get_autostart(State(_state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "installed": autostart::is_installed(),
        "supported": cfg!(any(target_os = "macos", target_os = "linux", target_os = "windows")),
        "platform": std::env::consts::OS,
        "location": autostart::install_location().map(|p| p.display().to_string()),
    }))
}

#[derive(Deserialize)]
struct AutostartBody {
    enable: bool,
}

async fn post_autostart(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AutostartBody>,
) -> Response {
    let result = if body.enable {
        autostart::install()
    } else {
        autostart::uninstall()
    };

    match result {
        Ok(()) => {
            let verb = if body.enable { "enabled" } else { "disabled" };
            state.log_event(EventLevel::Ok, format!("autostart {}", verb));
            Json(serde_json::json!({
                "ok": true,
                "installed": autostart::is_installed(),
            }))
            .into_response()
        }
        Err(e) => {
            state.log_event(EventLevel::Err, format!("autostart change failed: {}", e));
            err_json(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        }
    }
}

// ============================================================================
// /api/calls
// ============================================================================

#[derive(Deserialize)]
struct CallsQuery {
    days: Option<i64>,
}

async fn get_calls(
    State(state): State<Arc<AppState>>,
    Query(q): Query<CallsQuery>,
) -> Json<serde_json::Value> {
    let days = q.days.unwrap_or(7).max(1);
    let db_opt = state.db.read().clone();
    let Some(db) = db_opt else {
        return Json(serde_json::json!({
            "calls": [],
            "summary": { "total_calls": 0, "total_duration_secs": 0, "avg_duration_secs": 0 }
        }));
    };
    let calls = db.list_calls(days).unwrap_or_default();
    let total_calls = calls.len() as u64;
    let total_duration_secs: i64 = calls.iter().filter_map(|c| c.duration_secs).sum();
    let avg = if total_calls > 0 {
        total_duration_secs / total_calls as i64
    } else {
        0
    };
    Json(serde_json::json!({
        "calls": calls,
        "summary": {
            "total_calls": total_calls,
            "total_duration_secs": total_duration_secs,
            "avg_duration_secs": avg,
        }
    }))
}
