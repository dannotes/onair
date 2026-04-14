use std::path::PathBuf;

/// Default Microsoft Teams log directory for the current platform.
/// User can override this via the Settings UI; this is just the auto-detected default.
pub fn default_teams_log_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(
            "Library/Group Containers/UBF8T346G9.com.microsoft.teams/Library/Application Support/Logs",
        ))
    }
    #[cfg(target_os = "windows")]
    {
        let local = std::env::var_os("LOCALAPPDATA")?;
        Some(
            PathBuf::from(local)
                .join(r"Packages\MSTeams_8wekyb3d8bbwe\LocalCache\Microsoft\MSTeams\Logs"),
        )
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".config/Microsoft/Microsoft Teams/logs"))
    }
}

/// Default application data directory (where onair.db lives).
pub fn default_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join("Library/Application Support/Onair")
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default();
        appdata.join("Onair")
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join(".config/onair")
    }
}

/// Default DB file path (data_dir + onair.db).
pub fn default_db_path() -> PathBuf {
    default_data_dir().join("onair.db")
}
