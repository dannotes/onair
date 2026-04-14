//! Cross-platform "run on login" helper.
//!
//! Lets the Settings UI install / uninstall onair as a login-time daemon
//! without the user touching launchd plists / systemd units / Windows
//! registry entries by hand.
//!
//! Per-OS strategy:
//!
//! - **macOS**: writes `~/Library/LaunchAgents/com.dannotes.onair.plist` and
//!   loads it via `launchctl load`. `KeepAlive=true` so onair restarts on
//!   crash. Logs to `/tmp/onair.log`.
//! - **Linux**: writes `~/.config/systemd/user/onair.service` and enables it
//!   via `systemctl --user enable --now`. Logs go to journald.
//! - **Windows**: drops a shortcut into the user's Startup folder
//!   (`%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\onair.lnk`).
//!   Created via a one-liner PowerShell `WScript.Shell` COM call so users can
//!   see it in Task Manager → Startup and remove it manually if desired. No
//!   admin required.
//!
//! The path to the running binary is resolved via `std::env::current_exe()`,
//! so the install adapts whether onair came from `brew`, `scoop`, a manual
//! download, or `cargo run`.

use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::process::Command;

/// True if onair is currently configured to start on login.
pub fn is_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        mac::is_installed()
    }
    #[cfg(target_os = "linux")]
    {
        linux::is_installed()
    }
    #[cfg(target_os = "windows")]
    {
        windows::is_installed()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}

/// Install onair as a login-time service for the current user.
pub fn install() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe_str = exe.to_string_lossy().into_owned();
    #[cfg(target_os = "macos")]
    {
        mac::install(&exe_str)
    }
    #[cfg(target_os = "linux")]
    {
        linux::install(&exe_str)
    }
    #[cfg(target_os = "windows")]
    {
        windows::install(&exe_str)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = exe_str;
        Err(anyhow!(
            "autostart is not supported on this operating system"
        ))
    }
}

/// Remove onair from the user's login-time services.
pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        mac::uninstall()
    }
    #[cfg(target_os = "linux")]
    {
        linux::uninstall()
    }
    #[cfg(target_os = "windows")]
    {
        windows::uninstall()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(anyhow!(
            "autostart is not supported on this operating system"
        ))
    }
}

/// Path that the autostart entry would be written to. Useful for the UI to
/// show "currently installed at: ..." next to the toggle.
pub fn install_location() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(mac::plist_path())
    }
    #[cfg(target_os = "linux")]
    {
        Some(linux::unit_path())
    }
    #[cfg(target_os = "windows")]
    {
        Some(windows::shortcut_path())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

// ============================================================================
// macOS
// ============================================================================

#[cfg(target_os = "macos")]
mod mac {
    use super::*;

    pub(super) fn plist_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join("Library/LaunchAgents/com.dannotes.onair.plist")
    }

    pub(super) fn is_installed() -> bool {
        plist_path().exists()
    }

    pub(super) fn install(exe_path: &str) -> Result<()> {
        let path = plist_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // XML escape the binary path (paths shouldn't normally contain & < >
        // but guard anyway).
        let exe_xml = exe_path
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");

        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.dannotes.onair</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_xml}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/onair.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/onair.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>
"#
        );

        std::fs::write(&path, plist)?;

        // Best-effort unload first in case it was loaded with a stale plist.
        let _ = Command::new("launchctl")
            .args(["unload", path.to_str().unwrap_or("")])
            .output();

        let out = Command::new("launchctl")
            .args(["load", path.to_str().unwrap_or("")])
            .output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!("launchctl load failed: {}", stderr.trim()));
        }
        Ok(())
    }

    pub(super) fn uninstall() -> Result<()> {
        let path = plist_path();
        if path.exists() {
            let _ = Command::new("launchctl")
                .args(["unload", path.to_str().unwrap_or("")])
                .output();
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
}

// ============================================================================
// Linux
// ============================================================================

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub(super) fn unit_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join(".config/systemd/user/onair.service")
    }

    pub(super) fn is_installed() -> bool {
        // Authoritative check: ask systemctl whether the unit is enabled.
        // Falls back to plain file-exists if systemctl isn't available.
        match Command::new("systemctl")
            .args(["--user", "is-enabled", "onair.service"])
            .output()
        {
            Ok(out) => out.status.success(),
            Err(_) => unit_path().exists(),
        }
    }

    pub(super) fn install(exe_path: &str) -> Result<()> {
        let path = unit_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let unit = format!(
            r#"# onair — auto-generated by the dashboard "Run on Login" toggle.
[Unit]
Description=onair — Microsoft Teams presence indicator for Philips WiZ bulbs
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exe_path}
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#
        );

        std::fs::write(&path, unit)?;

        // Reload systemd, then enable + start.
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();

        let out = Command::new("systemctl")
            .args(["--user", "enable", "--now", "onair.service"])
            .output()
            .map_err(|e| anyhow!("systemctl --user enable failed to spawn: {}", e))?;
        if !out.status.success() {
            return Err(anyhow!(
                "systemctl --user enable --now failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    pub(super) fn uninstall() -> Result<()> {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", "onair.service"])
            .output();
        let path = unit_path();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .output();
        Ok(())
    }
}

// ============================================================================
// Windows
// ============================================================================

#[cfg(target_os = "windows")]
mod windows {
    use super::*;

    /// Resolve `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\onair.lnk`.
    pub(super) fn shortcut_path() -> PathBuf {
        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default();
        appdata.join(r"Microsoft\Windows\Start Menu\Programs\Startup\onair.lnk")
    }

    pub(super) fn is_installed() -> bool {
        shortcut_path().exists()
    }

    pub(super) fn install(exe_path: &str) -> Result<()> {
        let lnk = shortcut_path();
        let lnk_str = lnk.to_string_lossy().into_owned();
        let working_dir = lnk
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Single-quoted PowerShell strings escape an embedded ' as ''.
        let esc = |s: &str| s.replace('\'', "''");

        let ps = format!(
            "$ws = New-Object -ComObject WScript.Shell; \
             $sc = $ws.CreateShortcut('{lnk}'); \
             $sc.TargetPath = '{exe}'; \
             $sc.WorkingDirectory = '{wd}'; \
             $sc.WindowStyle = 7; \
             $sc.Description = 'onair — Teams presence bulb'; \
             $sc.Save()",
            lnk = esc(&lnk_str),
            exe = esc(exe_path),
            wd = esc(&working_dir),
        );

        let out = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .output()
            .map_err(|e| anyhow!("powershell failed to spawn: {}", e))?;
        if !out.status.success() {
            return Err(anyhow!(
                "powershell shortcut create failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    pub(super) fn uninstall() -> Result<()> {
        let lnk = shortcut_path();
        if lnk.exists() {
            std::fs::remove_file(&lnk)?;
        }
        Ok(())
    }
}
