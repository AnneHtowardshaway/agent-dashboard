//! Filesystem locations shared by the collector, server and CDP bridge.

use std::path::PathBuf;

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn data_dir() -> PathBuf {
    home().join(".gemini/antigravity/sidecar_data/antigravity-pulse")
}

pub fn run_dir() -> PathBuf {
    data_dir().join("run")
}

pub fn pid_file() -> PathBuf {
    run_dir().join("ag-pulse.pid")
}

pub fn log_file() -> PathBuf {
    data_dir().join("logs/ag-pulse.log")
}

pub fn preferences_file() -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support/AntigravityPulse/preferences.json")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData/Local"))
            .join("AntigravityPulse/preferences.json")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("AntigravityPulse/preferences.json")
    }
}
