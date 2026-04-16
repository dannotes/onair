//! Application state engine.
//!
//! Owns all live state shared between the monitor loop, the bulb poller, and
//! the web layer. Holds:
//! - mutable [`Config`] (eventually backed by SQLite — chunk 6)
//! - resolved bulb IP, latest `getPilot` snapshot
//! - current Teams presence + grace timer
//! - manual override mode
//! - a bounded ring buffer of [`Event`]s exposed via `/api/logs`
//! - per-day call statistics
//!
//! The two background tasks are [`monitor_loop`] (reads the Teams log and
//! drives the bulb based on presence changes) and [`bulb_poll_loop`] (polls
//! `getPilot` every 5 s so the dashboard beacon shows live bulb color).

use crate::bulb::{self, BulbState, DiscoveredBulb};
use crate::config::Db;
use crate::models::{Presence, Rgb, TriggerMode};
use crate::platform;
use crate::presence::LogWatcher;
use chrono::{DateTime, Local, NaiveDate, Timelike};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Maximum number of events we keep in the in-memory ring buffer. The web UI
/// can fetch up to this many via `/api/logs`. Older events are pruned when
/// the buffer is full.
const EVENT_BUFFER_CAP: usize = 1000;

/// How often the bulb-state poller calls `getPilot`. Independent of the Teams
/// poll interval — this is for the live UI beacon.
const BULB_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often the bulb-resolver retries to discover the configured MAC if the
/// bulb is unreachable on startup.
const BULB_DISCOVERY_RETRY_DELAY: Duration = Duration::from_secs(2);
const BULB_DISCOVERY_RETRIES: u32 = 3;

// ============================================================================
// Config
// ============================================================================

/// User-tunable configuration. Defaults match `PROJECT.md`. In chunk 6 this
/// is read/written through SQLite; for now it lives in memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub bulb_mac: String, // empty = no bulb selected
    /// Last successfully resolved IP for the configured bulb. Used at startup
    /// to skip broadcast discovery (which is unreliable on some networks /
    /// after a bulb has already been claimed by another app). Re-resolved if
    /// the cached IP doesn't respond.
    pub bulb_last_ip: String, // empty = unknown
    pub bulb_port: u16,
    pub teams_log_dir: Option<PathBuf>, // None = auto-detect via platform
    pub work_start: u8,                 // 0..=23
    pub work_end: u8,                   // 0..=23
    pub poll_interval_secs: u64,
    pub grace_period_secs: u64,
    pub max_call_hours: u64,
    pub teams_offline_mins: u64,
    pub call_state: BulbMode, // on = show color, off = bulb off
    pub call_color: Rgb,
    pub call_brightness: u8, // 10..=100
    pub idle_state: BulbMode,
    pub idle_color: Rgb,
    pub idle_brightness: u8,
    pub ui_port: u16,
    pub log_level: String,
    /// What presence state(s) should activate the bulb.
    pub trigger_mode: TriggerMode,
    /// Set to `true` after the first successful bulb resolve. Used to drive
    /// the opt-out "auto-enable autostart on first run" behavior in `main.rs`.
    pub first_run_completed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BulbMode {
    On,
    Off,
}

impl Config {
    pub fn defaults() -> Self {
        Self {
            bulb_mac: String::new(),
            bulb_last_ip: String::new(),
            bulb_port: 38899,
            teams_log_dir: None,
            work_start: 6,
            work_end: 18,
            poll_interval_secs: 3,
            grace_period_secs: 10,
            max_call_hours: 4,
            teams_offline_mins: 5,
            call_state: BulbMode::On,
            call_color: Rgb::new(0xFF, 0x00, 0x00),
            call_brightness: 100,
            idle_state: BulbMode::Off,
            idle_color: Rgb::new(0x22, 0xC5, 0x5E),
            idle_brightness: 50,
            ui_port: 9876,
            log_level: "info".to_string(),
            trigger_mode: TriggerMode::BusyAndDnd,
            first_run_completed: false,
        }
    }
}

// ============================================================================
// Override / Bulb display state
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideMode {
    Auto,
    ForceRed,
    ForceOff,
}

/// What the application *thinks* it has told the bulb to display. Distinct from
/// the live `BulbState` (which is what the bulb actually reports via `getPilot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayState {
    Off,
    Call, // showing call config
    Idle, // showing idle config (for "always-on ambient" setups)
}

// ============================================================================
// Event log
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EventLevel {
    Dbg,
    Inf,
    Ok,
    Wrn,
    Err,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub ts: DateTime<Local>,
    pub level: EventLevel,
    pub message: String,
}

// ============================================================================
// Call statistics
// ============================================================================

/// Per-day rolling stats. Resets on date change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DayStats {
    pub date: Option<NaiveDate>,
    pub calls_today: u32,
    pub total_time_today_secs: u64,
}

impl DayStats {
    fn ensure_today(&mut self) {
        let today = Local::now().date_naive();
        if self.date != Some(today) {
            self.date = Some(today);
            self.calls_today = 0;
            self.total_time_today_secs = 0;
        }
    }
}

// ============================================================================
// AppState
// ============================================================================

pub struct AppState {
    pub config: RwLock<Config>,

    /// Resolved IP of the configured bulb, if any.
    pub bulb_ip: RwLock<Option<Ipv4Addr>>,
    /// Last successful `getPilot` snapshot (from the bulb-poll task).
    pub bulb_live: RwLock<Option<BulbState>>,
    pub bulb_last_polled: RwLock<Option<DateTime<Local>>>,
    pub bulb_reachable: RwLock<bool>,

    /// What the app last commanded the bulb to display.
    pub display: RwLock<DisplayState>,

    pub current_presence: RwLock<Presence>,
    pub call_start: RwLock<Option<DateTime<Local>>>,

    /// If set, the time at which the grace period expires.
    /// While Some, we are in the busy→available grace window.
    pub grace_until: RwLock<Option<Instant>>,

    pub override_mode: RwLock<OverrideMode>,

    pub started_at: DateTime<Local>,
    pub stats: RwLock<DayStats>,

    /// Bounded ring buffer of recent events (newest at the back).
    events: RwLock<VecDeque<Event>>,
    next_event_id: RwLock<u64>,

    /// True while TeamsCallTracker reports an active call (Windows only).
    /// Used by `TriggerMode::CallOnly` to distinguish real calls from manual
    /// Busy status changes. Always `false` on macOS (no TeamsCallTracker).
    pub call_tracker_active: RwLock<bool>,

    /// Most recent discovery scan result.
    pub last_discovery: RwLock<Vec<DiscoveredBulb>>,

    /// Optional SQLite store. None during early-boot before the DB is opened,
    /// or if the DB couldn't be created.
    pub db: RwLock<Option<Arc<Db>>>,

    /// Active call's row id in the `calls` table, if any.
    pub current_call_id: RwLock<Option<i64>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config: RwLock::new(config),
            bulb_ip: RwLock::new(None),
            bulb_live: RwLock::new(None),
            bulb_last_polled: RwLock::new(None),
            bulb_reachable: RwLock::new(false),
            display: RwLock::new(DisplayState::Off),
            current_presence: RwLock::new(Presence::Unknown),
            call_start: RwLock::new(None),
            grace_until: RwLock::new(None),
            override_mode: RwLock::new(OverrideMode::Auto),
            started_at: Local::now(),
            stats: RwLock::new(DayStats::default()),
            events: RwLock::new(VecDeque::with_capacity(EVENT_BUFFER_CAP)),
            next_event_id: RwLock::new(1),
            call_tracker_active: RwLock::new(false),
            last_discovery: RwLock::new(Vec::new()),
            db: RwLock::new(None),
            current_call_id: RwLock::new(None),
        }
    }

    /// Persist the current config to the SQLite store. No-op if no DB is set.
    pub fn persist_config(&self) {
        let db_opt = self.db.read().clone();
        if let Some(db) = db_opt {
            let snap = self.config.read().clone();
            if let Err(e) = db.save_config(&snap) {
                warn!("save_config failed: {}", e);
            }
        }
    }

    /// Append an event to the in-memory ring buffer AND emit it via tracing
    /// (so it shows up in the developer console too).
    pub fn log_event(&self, level: EventLevel, message: impl Into<String>) {
        let message = message.into();
        match level {
            EventLevel::Inf => info!("{}", message),
            EventLevel::Ok => info!("[ok] {}", message),
            EventLevel::Wrn => warn!("{}", message),
            EventLevel::Err => tracing::error!("{}", message),
            EventLevel::Dbg => debug!("{}", message),
        }

        let mut id_guard = self.next_event_id.write();
        let id = *id_guard;
        *id_guard += 1;
        drop(id_guard);

        let event = Event {
            id,
            ts: Local::now(),
            level,
            message,
        };

        let mut events = self.events.write();
        if events.len() >= EVENT_BUFFER_CAP {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// Push a debug-level event to the ring buffer — only when `log_level`
    /// is `"debug"`. Always emits via `tracing::debug` regardless.
    pub fn log_debug(&self, message: impl Into<String>) {
        let message = message.into();
        debug!("{}", message);
        if self.config.read().log_level != "debug" {
            return;
        }
        let mut id_guard = self.next_event_id.write();
        let id = *id_guard;
        *id_guard += 1;
        drop(id_guard);
        let event = Event {
            id,
            ts: Local::now(),
            level: EventLevel::Dbg,
            message,
        };
        let mut events = self.events.write();
        if events.len() >= EVENT_BUFFER_CAP {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// True if the given incoming presence event should turn the light on,
    /// given the configured trigger mode. `is_call_event` is `true` only for
    /// TeamsCallTracker events on Windows.
    pub fn event_triggers(&self, presence: Presence, is_call_event: bool) -> bool {
        let mode = self.config.read().trigger_mode;
        match mode {
            TriggerMode::CallOnly => {
                if cfg!(target_os = "macos") {
                    // No call-tracker on Mac; Busy is the best proxy.
                    presence.is_in_call()
                } else {
                    // Windows: only a TeamsCallTracker call-active event counts.
                    // GlyphBadge Busy (manual status change) is intentionally ignored.
                    is_call_event && presence != Presence::Available
                }
            }
            TriggerMode::BusyAndDnd => presence.is_in_call(),
            TriggerMode::AnyNonAvailable => !matches!(
                presence,
                Presence::Available | Presence::Offline | Presence::Unknown
            ),
        }
    }

    /// True if the *current* state (presence + call_tracker_active) should
    /// have the light on. Used for grace-timer checks and reconcile_display.
    pub fn currently_triggered(&self) -> bool {
        let mode = self.config.read().trigger_mode;
        match mode {
            TriggerMode::CallOnly => {
                if cfg!(target_os = "macos") {
                    self.current_presence.read().is_in_call()
                } else {
                    *self.call_tracker_active.read()
                }
            }
            TriggerMode::BusyAndDnd => self.current_presence.read().is_in_call(),
            TriggerMode::AnyNonAvailable => {
                let p = *self.current_presence.read();
                !matches!(
                    p,
                    Presence::Available | Presence::Offline | Presence::Unknown
                )
            }
        }
    }

    /// Fetch the most recent `limit` events (newest last). `offset` skips
    /// from the newest end.
    pub fn get_events(&self, limit: usize, offset: usize) -> Vec<Event> {
        let events = self.events.read();
        let total = events.len();
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);
        events.range(start..end).cloned().collect()
    }

    pub fn total_events(&self) -> usize {
        self.events.read().len()
    }

    /// True if the current local time is inside the configured work window.
    /// Window is `[work_start, work_end)`. If they're equal, treats as 24h on.
    pub fn within_work_hours(&self) -> bool {
        let cfg = self.config.read();
        let now = Local::now();
        let h = now.hour() as u8;
        if cfg.work_start == cfg.work_end {
            return true;
        }
        if cfg.work_start < cfg.work_end {
            h >= cfg.work_start && h < cfg.work_end
        } else {
            // wraps midnight (e.g. 22..6)
            h >= cfg.work_start || h < cfg.work_end
        }
    }
}

// ============================================================================
// Bulb resolution
// ============================================================================

/// Try to find the bulb whose MAC matches the configured one. Updates
/// [`AppState::bulb_ip`] on success. Logs and gives up after retries on failure.
pub async fn resolve_bulb(state: Arc<AppState>) {
    let (mac, cached_ip) = {
        let cfg = state.config.read();
        (cfg.bulb_mac.clone(), cfg.bulb_last_ip.clone())
    };
    if mac.is_empty() {
        state.log_event(
            EventLevel::Wrn,
            "no bulb MAC configured — open Settings to select one",
        );
        *state.bulb_ip.write() = None;
        return;
    }

    // Try the cached IP first via unicast getPilot. WiZ broadcast discovery
    // is unreliable on some networks and after the bulb has been claimed by
    // another app, so cached IPs are MUCH more reliable.
    if let Ok(ip) = cached_ip.parse::<Ipv4Addr>() {
        match bulb::get_pilot(ip).await {
            Ok(_) => {
                *state.bulb_ip.write() = Some(ip);
                *state.bulb_reachable.write() = true;
                state.log_event(
                    EventLevel::Ok,
                    format!("reconnected to cached bulb {} at {}", mac, ip),
                );
                return;
            }
            Err(e) => {
                state.log_event(
                    EventLevel::Inf,
                    format!(
                        "cached IP {} unreachable ({}), falling back to discovery",
                        ip, e
                    ),
                );
            }
        }
    }

    for attempt in 1..=BULB_DISCOVERY_RETRIES {
        match bulb::discover(Duration::from_secs(3)).await {
            Ok(bulbs) => {
                *state.last_discovery.write() = bulbs.clone();
                if let Some(found) = bulbs.iter().find(|b| b.mac.eq_ignore_ascii_case(&mac)) {
                    *state.bulb_ip.write() = Some(found.ip);
                    *state.bulb_reachable.write() = true;
                    // Cache the IP for next startup.
                    state.config.write().bulb_last_ip = found.ip.to_string();
                    state.persist_config();
                    state.log_event(
                        EventLevel::Ok,
                        format!("found bulb {} at {}", found.mac, found.ip),
                    );
                    return;
                } else {
                    state.log_event(
                        EventLevel::Wrn,
                        format!(
                            "configured bulb {} not found (attempt {}/{}), {} other bulb(s) on LAN",
                            mac,
                            attempt,
                            BULB_DISCOVERY_RETRIES,
                            bulbs.len()
                        ),
                    );
                }
            }
            Err(e) => {
                state.log_event(
                    EventLevel::Wrn,
                    format!("discovery failed (attempt {}): {}", attempt, e),
                );
            }
        }
        tokio::time::sleep(BULB_DISCOVERY_RETRY_DELAY).await;
    }

    state.log_event(
        EventLevel::Err,
        format!(
            "could not find bulb {} after {} attempts",
            mac, BULB_DISCOVERY_RETRIES
        ),
    );
    *state.bulb_ip.write() = None;
    *state.bulb_reachable.write() = false;
}

// ============================================================================
// Bulb commands (centralized — applies override + display tracking)
// ============================================================================

/// Apply the configured "call" display (color or off). Updates `display`.
pub(crate) async fn apply_call(state: &Arc<AppState>) {
    let (mode, color, brightness) = {
        let cfg = state.config.read();
        (cfg.call_state, cfg.call_color, cfg.call_brightness)
    };
    apply_mode(state, mode, color, brightness, DisplayState::Call, "call").await;
}

/// Apply the configured "idle" display. Updates `display`.
pub(crate) async fn apply_idle(state: &Arc<AppState>) {
    let (mode, color, brightness) = {
        let cfg = state.config.read();
        (cfg.idle_state, cfg.idle_color, cfg.idle_brightness)
    };
    apply_mode(state, mode, color, brightness, DisplayState::Idle, "idle").await;
}

async fn apply_mode(
    state: &Arc<AppState>,
    mode: BulbMode,
    color: Rgb,
    brightness: u8,
    display: DisplayState,
    label: &str,
) {
    let Some(ip) = *state.bulb_ip.read() else {
        debug!("apply_mode({}): no bulb_ip, skipping", label);
        return;
    };
    let result = match mode {
        BulbMode::On => bulb::set_pilot_color(ip, color, brightness).await,
        BulbMode::Off => bulb::set_pilot_off(ip).await,
    };
    match result {
        Ok(_) => {
            *state.display.write() = display;
            *state.bulb_reachable.write() = true;
            // Eagerly mirror the new bulb state into bulb_live so the dashboard
            // doesn't show stale data until the next 5s getPilot poll. Preserve
            // the carried-over fields (rssi, temp, scene_id) from the previous
            // snapshot if available; default them otherwise.
            let prev: Option<BulbState> = state.bulb_live.read().clone();
            let new_live = match mode {
                BulbMode::On => BulbState {
                    state: true,
                    r: color.r,
                    g: color.g,
                    b: color.b,
                    dimming: brightness,
                    temp: prev.as_ref().map(|p| p.temp).unwrap_or(0),
                    rssi: prev.as_ref().map(|p| p.rssi).unwrap_or(0),
                    scene_id: prev.as_ref().map(|p| p.scene_id).unwrap_or(0),
                },
                BulbMode::Off => BulbState {
                    state: false,
                    r: prev.as_ref().map(|p| p.r).unwrap_or(0),
                    g: prev.as_ref().map(|p| p.g).unwrap_or(0),
                    b: prev.as_ref().map(|p| p.b).unwrap_or(0),
                    dimming: prev.as_ref().map(|p| p.dimming).unwrap_or(0),
                    temp: prev.as_ref().map(|p| p.temp).unwrap_or(0),
                    rssi: prev.as_ref().map(|p| p.rssi).unwrap_or(0),
                    scene_id: prev.as_ref().map(|p| p.scene_id).unwrap_or(0),
                },
            };
            *state.bulb_live.write() = Some(new_live);
            *state.bulb_last_polled.write() = Some(Local::now());

            match mode {
                BulbMode::On => state.log_event(
                    EventLevel::Ok,
                    format!(
                        "bulb -> {} {} brightness {}",
                        label.to_uppercase(),
                        color.to_hex(),
                        brightness
                    ),
                ),
                BulbMode::Off => state.log_event(
                    EventLevel::Ok,
                    format!("bulb -> {} OFF", label.to_uppercase()),
                ),
            }
        }
        Err(e) => {
            *state.bulb_reachable.write() = false;
            state.log_event(
                EventLevel::Err,
                format!("bulb command failed: {} (will retry next cycle)", e),
            );
        }
    }
}

/// Force the bulb fully off, regardless of mode/config. Used by Ctrl+C and
/// `force_off` override.
pub async fn force_off(state: &Arc<AppState>) {
    let Some(ip) = *state.bulb_ip.read() else {
        return;
    };
    if let Err(e) = bulb::set_pilot_off(ip).await {
        warn!("force_off failed: {}", e);
    } else {
        *state.display.write() = DisplayState::Off;
    }
}

/// Force the bulb red (call config), regardless of presence. Used by `force_red`
/// override and the "Force Red" UI button.
pub async fn force_red(state: &Arc<AppState>) {
    apply_call(state).await;
}

/// Re-evaluate what the bulb should be displaying based on current
/// (presence, call_start, override, work_hours). Used after the user flips
/// override back to Auto so the bulb immediately reflects what auto would do
/// instead of waiting for the next presence transition.
pub async fn reconcile_display(state: &Arc<AppState>) {
    let override_mode = *state.override_mode.read();
    match override_mode {
        OverrideMode::ForceRed => {
            apply_call(state).await;
            return;
        }
        OverrideMode::ForceOff => {
            force_off(state).await;
            return;
        }
        OverrideMode::Auto => {}
    }

    let call_start_some = state.call_start.read().is_some();
    let in_call = state.currently_triggered();

    if in_call && state.within_work_hours() {
        apply_call(state).await;
    } else if call_start_some {
        // We were on a call (auto-state remembers it) but presence is no
        // longer in_call. Treat this as the call having ended.
        *state.call_start.write() = None;
        *state.grace_until.write() = None;
        apply_idle(state).await;
    } else {
        apply_idle(state).await;
    }
}

// ============================================================================
// Monitor loop
// ============================================================================

/// Long-running task. Polls the Teams log and drives the bulb based on
/// presence changes, work hours, override, and grace timer.
pub async fn monitor_loop(state: Arc<AppState>) {
    let log_dir = state
        .config
        .read()
        .teams_log_dir
        .clone()
        .or_else(platform::default_teams_log_dir);

    let Some(log_dir) = log_dir else {
        state.log_event(
            EventLevel::Err,
            "could not determine Teams log directory for this OS — set it in Settings",
        );
        return;
    };

    state.log_event(
        EventLevel::Inf,
        format!("watching Teams log at {}", log_dir.display()),
    );
    let mut watcher = LogWatcher::new(log_dir);

    let poll_interval = Duration::from_secs(state.config.read().poll_interval_secs);
    loop {
        // Override short-circuits everything.
        let override_mode = *state.override_mode.read();
        match override_mode {
            OverrideMode::ForceRed => {
                if *state.display.read() != DisplayState::Call {
                    apply_call(&state).await;
                }
                tokio::time::sleep(poll_interval).await;
                continue;
            }
            OverrideMode::ForceOff => {
                if *state.display.read() != DisplayState::Off {
                    force_off(&state).await;
                }
                tokio::time::sleep(poll_interval).await;
                continue;
            }
            OverrideMode::Auto => {}
        }

        // 1. Pull new presence events.
        let events = match watcher.poll().await {
            Ok(ev) => ev,
            Err(e) => {
                debug!("presence poll error: {}", e);
                tokio::time::sleep(poll_interval).await;
                continue;
            }
        };

        if events.is_empty() {
            state.log_debug(format!(
                "presence poll: no new events (presence={:?} call_tracker_active={} triggered={})",
                *state.current_presence.read(),
                *state.call_tracker_active.read(),
                state.currently_triggered(),
            ));
        } else {
            state.log_debug(format!("presence poll: {} new event(s)", events.len()));
        }

        // 2. Process each new event in order.
        for ev in events {
            let prev = *state.current_presence.read();

            // Update call_tracker_active for Windows CallOnly mode.
            if ev.is_call_event {
                let active = ev.presence != Presence::Available;
                *state.call_tracker_active.write() = active;
                state.log_debug(format!(
                    "TeamsCallTracker: call_tracker_active → {} (raw={})",
                    active, ev.raw,
                ));
            }

            *state.current_presence.write() = ev.presence;

            // Skip if presence didn't change AND this isn't a call-tracker event.
            // Call-tracker events must be processed even when presence stays Busy
            // (e.g. manual Busy was already set before the call started).
            if prev == ev.presence && !ev.is_call_event {
                state.log_debug(format!(
                    "presence unchanged ({:?}), no call-tracker event, skipping",
                    ev.presence,
                ));
                continue;
            }

            let now_triggered = state.event_triggers(ev.presence, ev.is_call_event);
            let call_was_active = state.call_start.read().is_some();

            if ev.presence != prev {
                state.log_event(
                    EventLevel::Inf,
                    format!("Teams presence: {:?} -> {:?}", prev, ev.presence),
                );
            }
            state.log_debug(format!(
                "trigger check: mode={:?} now_triggered={} call_was_active={} (raw={} is_call_event={})",
                state.config.read().trigger_mode,
                now_triggered,
                call_was_active,
                ev.raw,
                ev.is_call_event,
            ));

            if now_triggered && !call_was_active {
                // available -> busy
                if !state.within_work_hours() {
                    state.log_event(
                        EventLevel::Inf,
                        "outside work hours — ignoring busy transition",
                    );
                    continue;
                }
                // Cancel any pending grace.
                *state.grace_until.write() = None;
                // Mark call start. Scope the write guard so it's dropped before await.
                {
                    let mut stats = state.stats.write();
                    stats.ensure_today();
                    stats.calls_today += 1;
                }
                let call_started = Local::now();
                *state.call_start.write() = Some(call_started);
                // Persist call start to DB (best effort).
                let db_opt = state.db.read().clone();
                if let Some(db) = db_opt {
                    match db.record_call_start(call_started) {
                        Ok(id) => *state.current_call_id.write() = Some(id),
                        Err(e) => warn!("record_call_start failed: {}", e),
                    }
                }
                apply_call(&state).await;
            } else if !now_triggered && call_was_active {
                // triggered -> not triggered — start grace timer.
                let grace = Duration::from_secs(state.config.read().grace_period_secs);
                *state.grace_until.write() = Some(Instant::now() + grace);
                state.log_event(
                    EventLevel::Inf,
                    format!("grace period started ({}s)", grace.as_secs()),
                );
            }
        }

        // 3. Grace timer expiry check.
        let grace_expired = {
            let g = state.grace_until.read();
            g.map(|t| Instant::now() >= t).unwrap_or(false)
        };
        if grace_expired {
            let still_triggered = state.currently_triggered();
            state.log_debug(format!(
                "grace expired — still_triggered={} (presence={:?} call_tracker_active={})",
                still_triggered,
                *state.current_presence.read(),
                *state.call_tracker_active.read(),
            ));
            if !still_triggered {
                let call_start_opt: Option<DateTime<Local>> = *state.call_start.read();
                if let Some(start) = call_start_opt {
                    let ended = Local::now();
                    let dur = (ended - start).num_seconds().max(0) as u64;
                    {
                        let mut stats = state.stats.write();
                        stats.ensure_today();
                        stats.total_time_today_secs += dur;
                    }
                    // Persist call end to DB.
                    let id_opt = *state.current_call_id.read();
                    let db_opt = state.db.read().clone();
                    if let (Some(id), Some(db)) = (id_opt, db_opt) {
                        if let Err(e) = db.record_call_end(id, ended, dur as i64) {
                            warn!("record_call_end failed: {}", e);
                        }
                    }
                    *state.current_call_id.write() = None;
                }
                *state.call_start.write() = None;
                *state.grace_until.write() = None;
                apply_idle(&state).await;
            } else {
                // State flipped back to triggered mid-grace — cancel.
                *state.grace_until.write() = None;
                state.log_event(EventLevel::Wrn, "grace cancelled — back-to-back call");
            }
        }

        // 4. Max call duration safety cap.
        let max_call_hours = state.config.read().max_call_hours;
        let call_start_opt: Option<DateTime<Local>> = *state.call_start.read();
        if let Some(start) = call_start_opt {
            let dur_h = (Local::now() - start).num_hours();
            if dur_h >= max_call_hours as i64 {
                state.log_event(
                    EventLevel::Wrn,
                    format!(
                        "call duration exceeded {}h cap — reverting to idle",
                        max_call_hours
                    ),
                );
                *state.call_start.write() = None;
                *state.grace_until.write() = None;
                apply_idle(&state).await;
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

// ============================================================================
// Bulb state poller
// ============================================================================

/// Long-running task. Polls `getPilot` every 5 s so the dashboard beacon can
/// render the bulb's actual live color.
pub async fn bulb_poll_loop(state: Arc<AppState>) {
    loop {
        // Read the IP into a local so the guard is not held across the await.
        let ip_opt: Option<Ipv4Addr> = *state.bulb_ip.read();
        if let Some(ip) = ip_opt {
            match bulb::get_pilot(ip).await {
                Ok(snapshot) => {
                    *state.bulb_live.write() = Some(snapshot);
                    *state.bulb_last_polled.write() = Some(Local::now());
                    *state.bulb_reachable.write() = true;
                }
                Err(e) => {
                    debug!("getPilot failed: {}", e);
                    *state.bulb_reachable.write() = false;
                }
            }
        }
        tokio::time::sleep(BULB_POLL_INTERVAL).await;
    }
}
