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
use std::collections::{HashMap, HashSet};
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

/// A long-lived watcher that tails every `MSTeams_*.log` file in a directory.
///
/// Multi-profile Teams (multiple work/school/personal accounts on the same
/// box) writes to several log files concurrently. Picking only the
/// mtime-latest file flips non-deterministically between profiles as each
/// writes chat heartbeats, dropping presence events on the floor. Instead
/// this watcher keeps a per-file position map and tails them all.
///
/// Detects log rotation (Teams sometimes creates a new log mid-day) and
/// new profile log files that appear mid-session. Reads only NEW bytes
/// since the last call; on the first call after construction it seeds
/// every existing log with its current size (seek-to-EOF semantics,
/// per-file) so we don't replay yesterday's status changes.
pub struct LogWatcher {
    log_dir: PathBuf,
    file_positions: HashMap<PathBuf, u64>,
    initialized: bool,
}

impl LogWatcher {
    /// Create a new watcher for the given log directory. Does not open any
    /// files — the first [`poll`](Self::poll) call enumerates every
    /// `MSTeams_*.log` and seeds each at EOF.
    pub fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            file_positions: HashMap::new(),
            initialized: false,
        }
    }

    /// Reads any new presence events since the last call, across all
    /// `MSTeams_*.log` files in the directory.
    ///
    /// - Empty Vec means no changes.
    /// - Handles log rotation and new-profile log files transparently.
    /// - Skips events that resolve to [`Presence::Unknown`] (too noisy).
    /// - Events within a single file are chronological. Events across files
    ///   in the same poll cycle come out in sorted-path order; since the
    ///   monitor loop applies them sequentially and the last-applied event
    ///   wins, cross-file ordering only matters if two profiles transition
    ///   within the same 3-second poll window, which is vanishingly rare.
    pub async fn poll(&mut self) -> Result<Vec<PresenceEvent>, PresenceError> {
        if !self.log_dir.exists() {
            return Err(PresenceError::LogDirNotFound(
                self.log_dir.display().to_string(),
            ));
        }

        let mut logs = list_log_files(&self.log_dir);
        if logs.is_empty() {
            return Err(PresenceError::NoLogFiles(
                self.log_dir.display().to_string(),
            ));
        }
        // Deterministic iteration order for any cross-file events.
        logs.sort();

        // First call ever: seed every log file with its current size so we
        // skip all existing backlog. Same strategy as the original
        // single-file design, just per-file.
        if !self.initialized {
            for path in &logs {
                let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                self.file_positions.insert(path.clone(), size);
            }
            info!(
                "LogWatcher initialized: tailing {} Teams log file(s) in {} (each seeked to end, skipping backlog)",
                logs.len(),
                self.log_dir.display()
            );
            self.initialized = true;
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        let present: HashSet<PathBuf> = logs.iter().cloned().collect();

        for path in &logs {
            // New file we haven't seen before — either a rotation or a new
            // Teams profile that just came online. Seed at EOF, same as
            // startup: we don't replay backlog.
            let last_pos = match self.file_positions.get(path).copied() {
                Some(p) => p,
                None => {
                    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    info!(
                        "new Teams log file appeared: {} (size={} bytes, seeking to end)",
                        path.display(),
                        size
                    );
                    self.file_positions.insert(path.clone(), size);
                    continue;
                }
            };

            let mut file = match File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    warn!("could not open {}: {}", path.display(), e);
                    continue;
                }
            };
            let size = match file.metadata() {
                Ok(m) => m.len(),
                Err(e) => {
                    warn!("could not stat {}: {}", path.display(), e);
                    continue;
                }
            };

            // Truncation guard: if the file shrank, rewind to 0 and keep going.
            let pos = if size < last_pos {
                warn!(
                    "Teams log {} shrank from {} to {} bytes; rewinding",
                    path.display(),
                    last_pos,
                    size
                );
                0
            } else {
                last_pos
            };

            if size == pos {
                continue;
            }

            if let Err(e) = file.seek(SeekFrom::Start(pos)) {
                warn!("seek failed on {}: {}", path.display(), e);
                continue;
            }
            let to_read = (size - pos) as usize;
            let mut buf = Vec::with_capacity(to_read);
            // We intentionally do NOT use read_to_string — Teams logs can
            // contain interspersed binary bytes that would fail UTF-8
            // validation. Lossy decode is good enough for line-oriented
            // regex matching.
            let read = match file.take(to_read as u64).read_to_end(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    warn!("read failed on {}: {}", path.display(), e);
                    continue;
                }
            };
            self.file_positions.insert(path.clone(), pos + read as u64);

            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if let Some(ev) = parse_line(line) {
                    if ev.presence == Presence::Unknown {
                        debug!("skipping Unknown presence event (raw={})", ev.raw);
                        continue;
                    }
                    debug!(
                        "presence event from {}: {:?} (raw={})",
                        path.display(),
                        ev.presence,
                        ev.raw
                    );
                    events.push(ev);
                }
            }
        }

        // Drop positions for files Teams cleaned up so the map doesn't
        // grow forever.
        self.file_positions.retain(|p, _| present.contains(p));

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

// Windows / Linux: two signal sources, checked in order.
//
// 1. `TeamsCallTracker: Call became active: <uuid> (total: N)` and
//    `TeamsCallTracker: Call ended: <uuid> (remaining: 0)` — direct
//    call counter that fires for every call on every profile,
//    regardless of whether that profile is foreground in the Teams
//    UI. This is the primary signal for multi-account setups because
//    `TaskbarBadgeServiceLegacy` only writes GlyphBadge events when
//    the Windows taskbar badge visibly changes, and the taskbar badge
//    only reflects ONE profile at a time — so calls in background
//    profiles never get a GlyphBadge event.
//
// 2. `TaskbarBadgeServiceLegacy: ... GlyphBadge{"..."}` — the
//    availability-state marker. Still needed because it catches
//    MANUAL Busy / DoNotDisturb transitions (a deliberate feature:
//    users can manually set Busy to trigger the bulb outside of a
//    call). It also covers Available / Away / BeRightBack / Offline
//    which TeamsCallTracker doesn't know about.
#[cfg(any(
    target_os = "windows",
    all(not(target_os = "macos"), not(target_os = "windows"))
))]
static WIN_CALL_ACTIVE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"TeamsCallTracker: Call became active:")
        .expect("windows call-active regex compiles")
});

#[cfg(any(
    target_os = "windows",
    all(not(target_os = "macos"), not(target_os = "windows"))
))]
static WIN_CALL_ENDED_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"TeamsCallTracker: Call ended:.*\(remaining:\s*0\s*\)")
        .expect("windows call-ended regex compiles")
});

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
    // Primary: TeamsCallTracker (fires for all profiles, foreground or not).
    if WIN_CALL_ACTIVE_RE.is_match(line) {
        return Some(PresenceEvent {
            presence: Presence::Busy,
            raw: "call-active".to_string(),
        });
    }
    if WIN_CALL_ENDED_RE.is_match(line) {
        return Some(PresenceEvent {
            presence: Presence::Available,
            raw: "call-ended".to_string(),
        });
    }
    // Secondary: GlyphBadge (foreground profile only, but catches
    // manual Busy/DoNotDisturb and the other availability states).
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

    #[cfg(any(
        target_os = "windows",
        all(not(target_os = "macos"), not(target_os = "windows"))
    ))]
    #[test]
    fn parses_windows_call_tracker_active() {
        // Fires even when the call is on a background profile that never
        // updates the taskbar badge — this is the multi-profile fix.
        let line = "2026-04-14T14:12:06.817968+05:30 0x00002134 <INFO> TeamsCallTracker: Call became active: fcd632d4-112d-4e9c-9b2f-3f9467b19641 (total: 1)";
        let ev = parse_line(line).expect("should match");
        assert_eq!(ev.presence, Presence::Busy);
        assert_eq!(ev.raw, "call-active");
    }

    #[cfg(any(
        target_os = "windows",
        all(not(target_os = "macos"), not(target_os = "windows"))
    ))]
    #[test]
    fn parses_windows_call_tracker_ended_zero_remaining() {
        let line = "2026-04-14T14:12:26.604803+05:30 0x00002134 <INFO> TeamsCallTracker: Call ended: fcd632d4-112d-4e9c-9b2f-3f9467b19641 (remaining: 0)";
        let ev = parse_line(line).expect("should match");
        assert_eq!(ev.presence, Presence::Available);
        assert_eq!(ev.raw, "call-ended");
    }

    #[cfg(any(
        target_os = "windows",
        all(not(target_os = "macos"), not(target_os = "windows"))
    ))]
    #[test]
    fn ignores_call_ended_when_other_calls_still_active() {
        // User is in two concurrent calls and ends one — we should stay
        // Busy, not flicker to Available. The regex requires `remaining: 0`
        // so this line must NOT match.
        let line = "2026-04-14T14:12:26.604803+05:30 0x00002134 <INFO> TeamsCallTracker: Call ended: fcd632d4-... (remaining: 1)";
        assert!(parse_line(line).is_none());
    }

    // Always-run syntax check for the Windows TeamsCallTracker regexes,
    // duplicated outside the cfg gate so that CI on any platform — and
    // local Mac `cargo test` — catches a typo before it ships.
    #[test]
    fn windows_call_tracker_regexes_compile_and_match() {
        let active = Regex::new(r"TeamsCallTracker: Call became active:")
            .expect("active regex must compile");
        let ended = Regex::new(r"TeamsCallTracker: Call ended:.*\(remaining:\s*0\s*\)")
            .expect("ended regex must compile");

        let call_active = "2026-04-14T14:12:06.817968+05:30 0x00002134 <INFO> TeamsCallTracker: Call became active: fcd632d4-112d-4e9c-9b2f-3f9467b19641 (total: 1)";
        let call_ended_zero = "2026-04-14T14:12:26.604803+05:30 0x00002134 <INFO> TeamsCallTracker: Call ended: fcd632d4-... (remaining: 0)";
        let call_ended_one = "2026-04-14T14:12:26.604803+05:30 0x00002134 <INFO> TeamsCallTracker: Call ended: fcd632d4-... (remaining: 1)";

        assert!(active.is_match(call_active));
        assert!(!active.is_match(call_ended_zero));
        assert!(ended.is_match(call_ended_zero));
        assert!(!ended.is_match(call_ended_one));
    }
}
