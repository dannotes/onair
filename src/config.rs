//! SQLite persistence layer.
//!
//! Stores user config (so settings survive restarts), call history, and a
//! cache of discovered bulbs. The schema follows `PROJECT.md`. The DB file
//! lives in the OS-appropriate app-data directory (see `platform::default_db_path`).
//!
//! All operations are synchronous (rusqlite). They're fast enough at the
//! scale of a single-user dashboard that we don't bother with `spawn_blocking`.

use crate::models::{Rgb, TriggerMode};
use crate::state::{BulbMode, Config};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Days of event history to keep. Older rows pruned at startup.
const EVENT_RETENTION_DAYS: i64 = 30;
/// Days of call history to keep. Older rows pruned at startup.
const CALL_RETENTION_DAYS: i64 = 90;

pub struct Db {
    conn: Mutex<Connection>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CallRow {
    pub id: i64,
    pub started_at: DateTime<Local>,
    pub ended_at: Option<DateTime<Local>>,
    pub duration_secs: Option<i64>,
}

impl Db {
    /// Open or create the SQLite database at the given path. Creates parent
    /// directories if needed and runs schema migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        conn.execute_batch(SCHEMA)?;
        info!("opened sqlite db at {}", path.display());
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    /// Load config from the `config` key/value table. Any missing keys fall
    /// back to [`Config::defaults`]. Returns the merged result.
    pub fn load_config(&self) -> Result<Config> {
        let mut cfg = Config::defaults();
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT key, value FROM config")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (k, v) = row?;
            apply_key(&mut cfg, &k, &v);
        }
        Ok(cfg)
    }

    /// Persist the entire config object as key/value rows (UPSERT).
    pub fn save_config(&self, cfg: &Config) -> Result<()> {
        let conn = self.conn.lock();
        let pairs = config_to_pairs(cfg);
        for (k, v) in pairs {
            conn.execute(
                "INSERT INTO config (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k, v],
            )?;
        }
        Ok(())
    }

    /// Record the start of a call; returns the new row id.
    pub fn record_call_start(&self, started: DateTime<Local>) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO calls (started_at) VALUES (?1)",
            params![started.to_rfc3339()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Mark a previously-started call as ended.
    pub fn record_call_end(
        &self,
        id: i64,
        ended: DateTime<Local>,
        duration_secs: i64,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE calls SET ended_at = ?1, duration_secs = ?2 WHERE id = ?3",
            params![ended.to_rfc3339(), duration_secs, id],
        )?;
        Ok(())
    }

    /// List calls within the last `days` days, newest first.
    pub fn list_calls(&self, days: i64) -> Result<Vec<CallRow>> {
        let cutoff = (Local::now() - chrono::Duration::days(days)).to_rfc3339();
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, started_at, ended_at, duration_secs
             FROM calls WHERE started_at >= ?1 ORDER BY started_at DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], |r| {
            let started: String = r.get(1)?;
            let ended: Option<String> = r.get(2)?;
            Ok(CallRow {
                id: r.get(0)?,
                started_at: parse_dt(&started).unwrap_or_else(Local::now),
                ended_at: ended.and_then(|s| parse_dt(&s)),
                duration_secs: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Best-effort prune of stale rows. Logs but doesn't fail on errors.
    pub fn prune_old(&self) {
        let conn = self.conn.lock();
        let event_cutoff =
            (Local::now() - chrono::Duration::days(EVENT_RETENTION_DAYS)).to_rfc3339();
        let call_cutoff = (Local::now() - chrono::Duration::days(CALL_RETENTION_DAYS)).to_rfc3339();
        if let Err(e) = conn.execute("DELETE FROM events WHERE ts < ?1", params![event_cutoff]) {
            warn!("prune events failed: {}", e);
        }
        if let Err(e) = conn.execute(
            "DELETE FROM calls WHERE started_at < ?1",
            params![call_cutoff],
        ) {
            warn!("prune calls failed: {}", e);
        }
    }
}

fn parse_dt(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

// ----------------------------------------------------------------------------
// Schema
// ----------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS calls (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at    TEXT NOT NULL,
    ended_at      TEXT,
    duration_secs INTEGER
);

CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    ts      TEXT NOT NULL,
    level   TEXT NOT NULL,
    message TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS bulbs (
    mac       TEXT PRIMARY KEY,
    ip        TEXT NOT NULL,
    module    TEXT,
    last_seen TEXT NOT NULL
);
"#;

// ----------------------------------------------------------------------------
// Config (de)serialization to flat key/value pairs
// ----------------------------------------------------------------------------

fn config_to_pairs(cfg: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("bulb_mac", cfg.bulb_mac.clone()),
        ("bulb_last_ip", cfg.bulb_last_ip.clone()),
        ("bulb_port", cfg.bulb_port.to_string()),
        (
            "teams_log_dir",
            cfg.teams_log_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        ("work_start", cfg.work_start.to_string()),
        ("work_end", cfg.work_end.to_string()),
        ("poll_interval_secs", cfg.poll_interval_secs.to_string()),
        ("grace_period_secs", cfg.grace_period_secs.to_string()),
        ("max_call_hours", cfg.max_call_hours.to_string()),
        ("teams_offline_mins", cfg.teams_offline_mins.to_string()),
        ("call_state", mode_to_str(cfg.call_state).into()),
        ("call_color", cfg.call_color.to_hex()),
        ("call_brightness", cfg.call_brightness.to_string()),
        ("idle_state", mode_to_str(cfg.idle_state).into()),
        ("idle_color", cfg.idle_color.to_hex()),
        ("idle_brightness", cfg.idle_brightness.to_string()),
        ("ui_port", cfg.ui_port.to_string()),
        ("log_level", cfg.log_level.clone()),
        (
            "trigger_mode",
            trigger_mode_to_str(cfg.trigger_mode).to_string(),
        ),
        (
            "first_run_completed",
            if cfg.first_run_completed { "1" } else { "0" }.to_string(),
        ),
    ]
}

fn apply_key(cfg: &mut Config, key: &str, value: &str) {
    match key {
        "bulb_mac" => cfg.bulb_mac = value.to_string(),
        "bulb_last_ip" => cfg.bulb_last_ip = value.to_string(),
        "bulb_port" => {
            if let Ok(v) = value.parse() {
                cfg.bulb_port = v;
            }
        }
        "teams_log_dir" => {
            cfg.teams_log_dir = if value.is_empty() {
                None
            } else {
                Some(PathBuf::from(value))
            };
        }
        "work_start" => {
            if let Ok(v) = value.parse() {
                cfg.work_start = v;
            }
        }
        "work_end" => {
            if let Ok(v) = value.parse() {
                cfg.work_end = v;
            }
        }
        "poll_interval_secs" => {
            if let Ok(v) = value.parse() {
                cfg.poll_interval_secs = v;
            }
        }
        "grace_period_secs" => {
            if let Ok(v) = value.parse() {
                cfg.grace_period_secs = v;
            }
        }
        "max_call_hours" => {
            if let Ok(v) = value.parse() {
                cfg.max_call_hours = v;
            }
        }
        "teams_offline_mins" => {
            if let Ok(v) = value.parse() {
                cfg.teams_offline_mins = v;
            }
        }
        "call_state" => cfg.call_state = mode_from_str(value).unwrap_or(cfg.call_state),
        "call_color" => {
            if let Some(rgb) = Rgb::from_hex(value) {
                cfg.call_color = rgb;
            }
        }
        "call_brightness" => {
            if let Ok(v) = value.parse() {
                cfg.call_brightness = v;
            }
        }
        "idle_state" => cfg.idle_state = mode_from_str(value).unwrap_or(cfg.idle_state),
        "idle_color" => {
            if let Some(rgb) = Rgb::from_hex(value) {
                cfg.idle_color = rgb;
            }
        }
        "idle_brightness" => {
            if let Ok(v) = value.parse() {
                cfg.idle_brightness = v;
            }
        }
        "ui_port" => {
            if let Ok(v) = value.parse() {
                cfg.ui_port = v;
            }
        }
        "log_level" => cfg.log_level = value.to_string(),
        "trigger_mode" => {
            cfg.trigger_mode = trigger_mode_from_str(value).unwrap_or(cfg.trigger_mode)
        }
        "first_run_completed" => cfg.first_run_completed = matches!(value, "1" | "true"),
        _ => {} // unknown key — ignore gracefully so old DBs stay forward-compatible
    }
}

fn mode_to_str(m: BulbMode) -> &'static str {
    match m {
        BulbMode::On => "on",
        BulbMode::Off => "off",
    }
}

fn mode_from_str(s: &str) -> Option<BulbMode> {
    match s {
        "on" => Some(BulbMode::On),
        "off" => Some(BulbMode::Off),
        _ => None,
    }
}

fn trigger_mode_to_str(m: TriggerMode) -> &'static str {
    match m {
        TriggerMode::CallOnly => "call_only",
        TriggerMode::BusyAndDnd => "busy_and_dnd",
        TriggerMode::AnyNonAvailable => "any_non_available",
    }
}

fn trigger_mode_from_str(s: &str) -> Option<TriggerMode> {
    match s {
        "call_only" => Some(TriggerMode::CallOnly),
        "busy_and_dnd" => Some(TriggerMode::BusyAndDnd),
        "any_non_available" => Some(TriggerMode::AnyNonAvailable),
        _ => None,
    }
}
