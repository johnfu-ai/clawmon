use crate::settings::Settings;
use crate::wsl::run_wsl_stdin;
use serde::{Deserialize, Serialize};

/// The embedded WSL-side detection script (single source of truth; it is
/// piped to `wsl.exe -e python3 -` at runtime).
pub const DETECT_SCRIPT: &str = include_str!("../../src-tauri/src/detect.py");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TmuxInfo {
    /// pane id, e.g. "%3"
    pub pane: String,
    pub session: String,
    /// window index, e.g. "0.1"
    pub window: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RawSession {
    pub pid: i32,
    pub cwd: String,
    pub tty: String,
    pub tmux: Option<TmuxInfo>,
    pub transcript: Option<String>,
    pub session_id: String,
    /// last transcript entry type: "assistant" | "user" | "attachment" | ...
    pub last_type: String,
    pub last_ts: String,
    pub idle_sec: Option<i64>,
    pub preview: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawStatus {
    pub now: String,
    pub now_epoch: f64,
    pub sessions: Vec<RawSession>,
}

/// Run the detection script inside WSL and parse its JSON output.
pub fn detect(settings: &Settings) -> Result<RawStatus, String> {
    let out = run_wsl_stdin(&settings.wsl_distro, &["python3", "-"], DETECT_SCRIPT)?;
    serde_json::from_str(&out).map_err(|e| format!("解析检测结果失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample() {
        let raw = r#"{
            "now": "2026-09-08T07:30:14.581925+00:00",
            "now_epoch": 1788864614.5,
            "sessions": [{
                "pid": 20447,
                "cwd": "/home/john/statebar",
                "tty": "/dev/pts/2",
                "tmux": {"pane": "%0", "session": "work", "window": "1.0"},
                "transcript": "/home/john/.claude/projects/x/abc.jsonl",
                "session_id": "abc",
                "last_type": "assistant",
                "last_ts": "2026-09-08T07:30:09.435Z",
                "idle_sec": 5,
                "preview": "hello"
            }]
        }"#;
        let st: RawStatus = serde_json::from_str(raw).unwrap();
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.sessions[0].tmux.as_ref().unwrap().pane, "%0");
        assert_eq!(st.sessions[0].idle_sec, Some(5));
    }

    #[test]
    fn parse_null_tmux() {
        let raw = r#"{
            "now": "x", "now_epoch": 1.0,
            "sessions": [{"pid": 1, "cwd": "/a", "tty": "", "tmux": null,
                "transcript": null, "session_id": "", "last_type": "",
                "last_ts": "", "idle_sec": null, "preview": ""}]
        }"#;
        let st: RawStatus = serde_json::from_str(raw).unwrap();
        assert!(st.sessions[0].tmux.is_none());
    }

    /// Live smoke test (only meaningful inside WSL).
    #[test]
    #[cfg(target_os = "linux")]
    fn live_detect() {
        let s = Settings::default();
        let st = detect(&s).expect("detect failed");
        println!("{} sessions", st.sessions.len());
    }
}
