use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// UI poll interval in seconds.
    pub poll_interval_secs: u64,
    /// Idle below this → green (actively working).
    pub idle_green_secs: i64,
    /// Last transcript entry not from assistant AND idle above this → red
    /// (likely stuck waiting on the API, e.g. usage-limit pause).
    pub blocked_after_secs: i64,
    /// Auto-send resume keys to the terminal after the wait expires.
    pub auto_continue: bool,
    /// How long to wait after a session turns red before auto-continuing.
    /// Default 5h = the Claude Code usage-limit window.
    pub wait_secs: u64,
    /// Keys sent to resume, whitespace separated tmux key names
    /// (e.g. "Enter", "C-c Enter", "a b Enter").
    pub resume_keys: String,
    /// Max auto-send attempts per block episode (retries every
    /// `retry_interval_secs` afterwards until the session recovers).
    pub max_sends: u32,
    pub retry_interval_secs: u64,
    /// WSL distro name; empty = default distro.
    pub wsl_distro: String,
    /// Hide to the system tray instead of exiting when the window is closed.
    pub close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            poll_interval_secs: 5,
            idle_green_secs: 120,
            blocked_after_secs: 300,
            auto_continue: true,
            wait_secs: 5 * 3600,
            resume_keys: "Enter".to_string(),
            max_sends: 3,
            retry_interval_secs: 600,
            wsl_distro: String::new(),
            close_to_tray: true,
        }
    }
}

impl Settings {
    pub fn load(path: &PathBuf) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &PathBuf) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, data).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub").join("settings.json");
        let s = Settings::default();
        s.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.wait_secs, 5 * 3600);
        assert_eq!(loaded.resume_keys, "Enter");
    }

    #[test]
    fn partial_json_keeps_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.json");
        std::fs::write(&path, r#"{"waitSecs":60}"#).unwrap();
        let s = Settings::load(&path);
        assert_eq!(s.wait_secs, 60);
        assert_eq!(s.max_sends, 3); // default preserved
    }
}
