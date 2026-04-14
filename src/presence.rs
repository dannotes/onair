//! Microsoft Teams presence parser.
//!
//! Tails the latest `MSTeams_*.log` file in the configured Teams log directory
//! and emits [`PresenceEvent`]s when the user's status changes.
//!
//! Different operating systems write presence to the log in different formats,
//! so the parser is OS-conditional. See the per-OS modules below for the
//! verified line shapes the regexes match.

use crate::models::Presence;
use once_cell::sync::Lazy;
use regex::Regex;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{debug, info, warn};

/// A presence event read from the log.
#[derive(Debug, Clone)]
pub struct PresenceEvent {
    pub presence: Presence,
    /// Raw matched value as it appeared in the log (e.g. "Busy" on mac, "busy" on windows).
    pub raw: String,
}

#[derive(thiserror::Error, Debug)]
pub enum PresenceError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("teams log directory not found: {0}")]
    LogDirNotFound(String),
    #[error("no MSTeams_*.log files found in {0}")]
    NoLogFiles(String),
}

/// A long-lived watcher that tails the latest `MSTeams_*.log` file in a directory.
///
/// Detects log rotation (Teams sometimes creates a new log mid-day) and re-opens.
/// Reads only NEW lines since the last call; on the first call after construction
/// it seeks to END of file (skipping stale data so we don't replay yesterday's
/// status changes).
pub struct LogWatcher {
    log_dir: PathBuf,
    current_file: Option<PathBuf>,
    file_pos: u64,
    initialized: bool,
}

impl LogWatcher {
    /// Create a new watcher for the given log directory. Does not open any
    /// file — the first [`poll`](Self::poll) call locates the latest log and
    /// seeks to its end.
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            current_file: None,
            file_pos: 0,
            initialized: false,
        }
    }

    /// Reads any new presence events since the last call.
    ///
    /// - Empty Vec means no changes.
    /// - Handles log rotation transparently.
    /// - Skips events that resolve to [`Presence::Unknown`] (too noisy).
    /// - Returns events in chronological (file) order.
    pub async fn poll(&mut self) -> Result<Vec<PresenceEvent>, PresenceError> {
        if !self.log_dir.exists() {
            return Err(PresenceError::LogDirNotFound(
                self.log_dir.display().to_string(),
            ));
        }

        // Find the freshest MSTeams_*.log file.
        let latest = match find_latest_log(&self.log_dir) {
            Some(p) => p,
            None => {
                return Err(PresenceError::NoLogFiles(
                    self.log_dir.display().to_string(),
                ));
            }
        };

        // First call ever: seek to end of latest file, skip any backlog.
        if !self.initialized {
            let size = fs::metadata(&latest)?.len();
            info!(
                "LogWatcher initialized on {} (size={} bytes, seeking to end)",
                latest.display(),
                size
            );
            self.current_file = Some(latest);
            self.file_pos = size;
            self.initialized = true;
            return Ok(Vec::new());
        }

        // Detect rotation: a different file is now the freshest.
        let rotated = match &self.current_file {
            Some(cur) => cur != &latest,
            None => true,
        };
        if rotated {
            info!(
                "Detected Teams log rotation: {:?} -> {}",
                self.current_file.as_ref().map(|p| p.display().to_string()),
                latest.display()
            );
            self.current_file = Some(latest.clone());
            self.file_pos = 0;
        }

        let path = self.current_file.as_ref().unwrap().clone();

        // Open and seek.
        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                warn!("could not open {}: {}", path.display(), e);
                return Err(PresenceError::Io(e));
            }
        };
        let size = file.metadata()?.len();

        // If the file shrank (truncated/replaced under us), restart from 0.
        if size < self.file_pos {
            warn!(
                "Teams log {} shrank from {} to {} bytes; rewinding",
                path.display(),
                self.file_pos,
                size
            );
            self.file_pos = 0;
        }

        if size == self.file_pos {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.file_pos))?;
        let to_read = (size - self.file_pos) as usize;
        let mut buf = Vec::with_capacity(to_read);
        // We intentionally do NOT use read_to_string — macOS Teams logs contain
        // interspersed binary bytes that would fail UTF-8 validation.
        let read = file.take(to_read as u64).read_to_end(&mut buf)?;
        self.file_pos += read as u64;

        let text = String::from_utf8_lossy(&buf);

        let mut events = Vec::new();
        for line in text.lines() {
            if let Some(ev) = parse_line(line) {
                if ev.presence == Presence::Unknown {
                    debug!("skipping Unknown presence event (raw={})", ev.raw);
                    continue;
                }
                debug!("presence event: {:?} (raw={})", ev.presence, ev.raw);
                events.push(ev);
            }
        }

        Ok(events)
    }

    /// Verify a directory looks like a valid Teams log dir.
    /// Used by the Settings UI "Verify" button.
    pub fn verify(dir: &Path) -> VerifyResult {
        let mut result = VerifyResult {
            dir_exists: false,
            log_files_count: 0,
            latest_log: None,
            latest_mtime: None,
            sample_match: None,
            error: None,
        };

        if !dir.exists() {
            result.error = Some(format!("directory does not exist: {}", dir.display()));
            return result;
        }
        result.dir_exists = true;

        let logs = list_log_files(dir);
        result.log_files_count = logs.len();

        let latest = logs
            .into_iter()
            .filter_map(|p| {
                let mtime = fs::metadata(&p).and_then(|m| m.modified()).ok()?;
                Some((p, mtime))
            })
            .max_by_key(|(_, mtime)| *mtime);

        if let Some((path, mtime)) = latest {
            result.latest_mtime = Some(mtime);

            // Try to extract a sample matching line from this file.
            match File::open(&path) {
                Ok(mut f) => {
                    let mut buf = Vec::new();
                    if let Err(e) = f.read_to_end(&mut buf) {
                        result.error = Some(format!("could not read {}: {}", path.display(), e));
                    } else {
                        let text = String::from_utf8_lossy(&buf);
                        for line in text.lines() {
                            if let Some(ev) = parse_line(line) {
                                // Use the first matched line as sample, even if Unknown.
                                result.sample_match =
                                    Some(format!("{:?}: {}", ev.presence, line.trim()));
                                let _ = ev;
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    result.error = Some(format!("could not open {}: {}", path.display(), e));
                }
            }
            result.latest_log = Some(path);
        } else if result.log_files_count == 0 {
            result.error = Some(format!("no MSTeams_*.log files in {}", dir.display()));
        }

        result
    }
}

#[derive(Debug)]
pub struct VerifyResult {
    pub dir_exists: bool,
    pub log_files_count: usize,
    pub latest_log: Option<PathBuf>,
    pub latest_mtime: Option<SystemTime>,
    pub sample_match: Option<String>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// File discovery
// ---------------------------------------------------------------------------

fn list_log_files(dir: &Path) -> Vec<PathBuf> {
    let pattern = dir.join("MSTeams_*.log");
    let pattern_str = match pattern.to_str() {
        Some(s) => s,
        None => return Vec::new(),
    };

    match glob::glob(pattern_str) {
        Ok(iter) => iter.filter_map(|res| res.ok()).collect(),
        Err(e) => {
            warn!("glob error for {}: {}", pattern_str, e);
            Vec::new()
        }
    }
}

fn find_latest_log(dir: &Path) -> Option<PathBuf> {
    list_log_files(dir)
        .into_iter()
        .filter_map(|p| {
            let mtime = fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((p, mtime))
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(p, _)| p)
}

// ---------------------------------------------------------------------------
// Per-OS line parsing
// ---------------------------------------------------------------------------

/// Try to parse a single log line into a [`PresenceEvent`].
/// Returns `None` if the line doesn't contain a presence marker.
fn parse_line(line: &str) -> Option<PresenceEvent> {
    #[cfg(target_os = "macos")]
    {
        parse_line_mac(line)
    }
    #[cfg(target_os = "windows")]
    {
        parse_line_windows(line)
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        // Best-effort: Teams on Linux is uncommon. Try the Windows GlyphBadge format.
        parse_line_windows(line)
    }
}

// macOS: lines like
//   ... native_modules::UserDataCrossCloudModule: ... { availability: Available, ... }
#[cfg(target_os = "macos")]
static MAC_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"UserDataCrossCloudModule.*availability:\s*(\w+)")
        .expect("mac presence regex compiles")
});

#[cfg(target_os = "macos")]
fn parse_line_mac(line: &str) -> Option<PresenceEvent> {
    if !line.contains("UserDataCrossCloudModule") || !line.contains("availability:") {
        return None;
    }
    let caps = MAC_RE.captures(line)?;
    let raw = caps.get(1)?.as_str().to_string();
    let presence = mac_raw_to_presence(&raw);
    Some(PresenceEvent { presence, raw })
}

#[cfg(target_os = "macos")]
fn mac_raw_to_presence(raw: &str) -> Presence {
    match raw {
        "Available" => Presence::Available,
        "Away" => Presence::Away,
        "Busy" => Presence::Busy,
        "DoNotDisturb" => Presence::DoNotDisturb,
        "Offline" => Presence::Offline,
        "PresenceUnknown" => Presence::Unknown,
        // macOS doesn't appear to emit BeRightBack here, but be defensive.
        "BeRightBack" => Presence::BeRightBack,
        _ => Presence::Unknown,
    }
}

// Windows / Linux fallback: lines like
//   ... SetBadge Setting badge: GlyphBadge{"busy"}, ...
#[cfg(any(
    target_os = "windows",
    all(not(target_os = "macos"), not(target_os = "windows"))
))]
static WIN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"GlyphBadge\{"(\w+)"\}"#).expect("windows presence regex compiles"));

#[cfg(any(
    target_os = "windows",
    all(not(target_os = "macos"), not(target_os = "windows"))
))]
fn parse_line_windows(line: &str) -> Option<PresenceEvent> {
    let caps = WIN_RE.captures(line)?;
    let raw = caps.get(1)?.as_str().to_string();
    let presence = win_raw_to_presence(&raw);
    Some(PresenceEvent { presence, raw })
}

#[cfg(any(
    target_os = "windows",
    all(not(target_os = "macos"), not(target_os = "windows"))
))]
fn win_raw_to_presence(raw: &str) -> Presence {
    match raw {
        "available" => Presence::Available,
        "busy" => Presence::Busy,
        "away" => Presence::Away,
        "berightback" => Presence::BeRightBack,
        "donotdisturb" => Presence::DoNotDisturb,
        "offline" => Presence::Offline,
        _ => Presence::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_mac_available_line() {
        let line = "2026-01-09T08:29:33.642191+05:30 0x00000001f8037100 <INFO> native_modules::UserDataCrossCloudModule: CloudStateChanged: New Cloud State Event: UserDataCloudState total number of users: 1 { availability: Available, unread notification count: 0 }";
        let ev = parse_line(line).expect("should match");
        assert_eq!(ev.presence, Presence::Available);
        assert_eq!(ev.raw, "Available");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_mac_busy_line() {
        let line = "2026-04-14T08:29:33.638503+05:30 0x00000001f8037100 <INFO> native_modules::UserDataCrossCloudModule: CloudStateChanged: New Cloud State Event: UserDataCloudState total number of users: 1 { availability: Busy, unread notification count: 0 }";
        let ev = parse_line(line).expect("should match");
        assert_eq!(ev.presence, Presence::Busy);
        assert!(ev.presence.is_in_call());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parses_mac_unknown_line() {
        let line = "2026-04-14T08:29:33.638503+05:30 0x00000001f8037100 <INFO> native_modules::UserDataCrossCloudModule: CloudStateChanged: New Cloud State Event: UserDataCloudState total number of users: 1 { availability: PresenceUnknown, unread notification count: 0 }";
        let ev = parse_line(line).expect("should match");
        assert_eq!(ev.presence, Presence::Unknown);
        assert_eq!(ev.raw, "PresenceUnknown");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ignores_unrelated_mac_line() {
        let line = "2026-01-09T05:58:32 some random log line with no presence";
        assert!(parse_line(line).is_none());
    }

    #[cfg(any(
        target_os = "windows",
        all(not(target_os = "macos"), not(target_os = "windows"))
    ))]
    #[test]
    fn parses_windows_busy_line() {
        let line = r#"2026-04-13T13:25:43.691087+05:30 0x00003d98 <DBG>  TaskbarBadgeServiceLegacy:Work: SetBadge Setting badge: GlyphBadge{"busy"}, overlay: No items, status Busy"#;
        let ev = parse_line(line).expect("should match");
        assert_eq!(ev.presence, Presence::Busy);
        assert_eq!(ev.raw, "busy");
    }
}
