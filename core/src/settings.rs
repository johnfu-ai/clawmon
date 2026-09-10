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

/// Bounds on the tmux key list (how many keys, and how long each may be).
const MAX_KEYS: usize = 8;
const MAX_KEY_LEN: usize = 20;
const MAX_DISTRO_LEN: usize = 64;

impl Settings {
    pub fn load(path: &PathBuf) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
            .unwrap_or_default()
            .sanitize()
    }

    pub fn save(&self, path: &PathBuf) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, data).map_err(|e| e.to_string())
    }

    /// Clamp every field into a usable range.
    ///
    /// `settings.json` is a plain file a user can hand-edit, and the UI is not
    /// the only writer, so nothing here can be trusted to be sane. A poll
    /// interval of `0` would spin the detector as fast as it can complete, and
    /// a resume key of `-R` would be parsed by tmux as an option rather than a
    /// key press.
    pub fn sanitize(mut self) -> Settings {
        self.poll_interval_secs = self.poll_interval_secs.clamp(2, 3600);
        self.idle_green_secs = self.idle_green_secs.clamp(5, 86_400);
        self.blocked_after_secs = self.blocked_after_secs.clamp(10, 86_400);
        // 0 is meaningful (send as soon as the session turns red)
        self.wait_secs = self.wait_secs.min(7 * 86_400);
        self.max_sends = self.max_sends.clamp(1, 100);
        self.retry_interval_secs = self.retry_interval_secs.clamp(5, 86_400);
        self.resume_keys = sanitize_keys(&self.resume_keys);
        self.wsl_distro = sanitize_distro(&self.wsl_distro);
        self
    }
}

fn sanitize_keys(raw: &str) -> String {
    let keys: Vec<&str> = raw
        .split_whitespace()
        // tmux would read a leading `-` as one of its own options
        .filter(|k| !k.starts_with('-') && k.len() <= MAX_KEY_LEN)
        .take(MAX_KEYS)
        .collect();
    if keys.is_empty() {
        "Enter".to_string()
    } else {
        keys.join(" ")
    }
}

/// A distro name is a single argument to `wsl.exe -d`; anything that looks
/// like it could be two arguments is dropped so the default distro is used.
fn sanitize_distro(raw: &str) -> String {
    let name = raw.trim();
    let clean = !name.is_empty()
        && name.len() <= MAX_DISTRO_LEN
        && !name.contains(char::is_whitespace)
        && !name.chars().any(char::is_control);
    if clean {
        name.to_string()
    } else {
        String::new()
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

    #[test]
    fn load_clamps_hand_edited_values() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.json");
        std::fs::write(
            &path,
            r#"{"pollIntervalSecs":0,"maxSends":0,"retryIntervalSecs":0,
                "idleGreenSecs":-5,"blockedAfterSecs":99999999}"#,
        )
        .unwrap();
        let s = Settings::load(&path);
        assert_eq!(s.poll_interval_secs, 2);
        assert_eq!(s.max_sends, 1);
        assert_eq!(s.retry_interval_secs, 5);
        assert_eq!(s.idle_green_secs, 5);
        assert_eq!(s.blocked_after_secs, 86_400);
    }

    #[test]
    fn resume_keys_never_look_like_tmux_options() {
        let s = Settings {
            resume_keys: "-R Enter".into(),
            ..Default::default()
        }
        .sanitize();
        assert_eq!(s.resume_keys, "Enter");

        let s = Settings {
            resume_keys: "  ".into(),
            ..Default::default()
        }
        .sanitize();
        assert_eq!(s.resume_keys, "Enter");

        let s = Settings {
            resume_keys: "C-c Enter".into(),
            ..Default::default()
        }
        .sanitize();
        assert_eq!(s.resume_keys, "C-c Enter");
    }

    #[test]
    fn distro_name_must_be_a_single_word() {
        let s = Settings {
            wsl_distro: "  Ubuntu  ".into(),
            ..Default::default()
        }
        .sanitize();
        assert_eq!(s.wsl_distro, "Ubuntu");

        let s = Settings {
            wsl_distro: "Ubuntu -e rm".into(),
            ..Default::default()
        }
        .sanitize();
        assert_eq!(s.wsl_distro, "");
    }
}
