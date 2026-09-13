use crate::settings::Settings;
use crate::wsl::{run_wsl_stdin, Persistent, CMD_TIMEOUT_SECS};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The embedded WSL-side detection script (single source of truth; it is
/// piped to `wsl.exe -e python3 -` at runtime).
pub const DETECT_SCRIPT: &str = include_str!("../../src-tauri/src/detect.py");

/// Where the script lands inside the distro for the resident process.
/// Uploaded again on every (re)spawn, so a volatile path is fine.
const SERVE_SCRIPT_PATH: &str = "/tmp/clawmon-detect.py";

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
    /// the last transcript entry is an assistant message with a tool_use
    /// block — a tool (e.g. a long Bash command) is still executing
    #[serde(default)]
    pub tool_running: bool,
    /// the transcript was written at or after this process started, so it is
    /// known to belong to it rather than being a stale file the fallback
    /// pairing heuristics handed us. Absent payloads default to trusted.
    #[serde(default = "trusted")]
    pub transcript_live: bool,
}

fn trusted() -> bool {
    true
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

/// Poll driver that keeps one resident detection process alive instead of
/// paying a fresh `wsl.exe` boot on every poll.
///
/// Any failure on the resident path (process died, request timed out,
/// unparsable reply) falls back to the classic one-shot pipe for that poll,
/// so a broken resident process can never cost us a poll — the next one
/// respawns it with a freshly uploaded script.
pub struct Detector {
    child: Option<Persistent>,
    /// distro the resident child was spawned for; a settings change must
    /// not keep us talking to a process in the wrong distro
    distro: String,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        Detector {
            child: None,
            distro: String::new(),
        }
    }

    pub fn detect(&mut self, settings: &Settings) -> Result<RawStatus, String> {
        if self.ensure_child(settings) {
            let child = self.child.as_mut().expect("ensure_child guarantees Some");
            // an unparseable reply (e.g. a crash trace) also breaks the
            // resident path and drops us to the one-shot below
            if let Ok(line) = child.request("run", Duration::from_secs(CMD_TIMEOUT_SECS)) {
                if let Ok(st) = serde_json::from_str(&line) {
                    return Ok(st);
                }
            }
            self.child = None;
        }
        detect(settings)
    }

    /// Make sure a resident process for `settings`' distro is running.
    fn ensure_child(&mut self, settings: &Settings) -> bool {
        if let Some(c) = &mut self.child {
            if self.distro == settings.wsl_distro && c.is_alive() {
                return true;
            }
        }
        self.child = None;
        match spawn_serve(settings) {
            Ok(c) => {
                self.distro = settings.wsl_distro.clone();
                self.child = Some(c);
                true
            }
            // python missing, WSL wedged, …: the one-shot path reports why
            Err(_) => false,
        }
    }
}

/// Upload the script and start it in serve mode.
fn spawn_serve(settings: &Settings) -> Result<Persistent, String> {
    run_wsl_stdin(
        &settings.wsl_distro,
        &["sh", "-c", &format!("cat > {SERVE_SCRIPT_PATH}")],
        DETECT_SCRIPT,
    )?;
    Persistent::spawn(
        &settings.wsl_distro,
        &["python3", "-u", SERVE_SCRIPT_PATH, "--serve"],
    )
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
                "preview": "hello",
                "tool_running": false,
                "transcript_live": true
            }]
        }"#;
        let st: RawStatus = serde_json::from_str(raw).unwrap();
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.sessions[0].tmux.as_ref().unwrap().pane, "%0");
        assert_eq!(st.sessions[0].idle_sec, Some(5));
        assert!(st.sessions[0].transcript_live);
    }

    /// An older detector does not send `transcript_live`; treat its records as
    /// trustworthy rather than silently dropping every session out of red.
    #[test]
    fn missing_transcript_live_defaults_to_trusted() {
        let raw = r#"{"now":"x","now_epoch":1.0,"sessions":[{"pid":1,"cwd":"/a",
            "tty":"","tmux":null,"transcript":"/t.jsonl","session_id":"",
            "last_type":"user","last_ts":"","idle_sec":5,"preview":""}]}"#;
        let st: RawStatus = serde_json::from_str(raw).unwrap();
        assert!(st.sessions[0].transcript_live);
        assert!(!st.sessions[0].tool_running);
    }

    #[test]
    fn parse_null_tmux() {
        let raw = r#"{
            "now": "x", "now_epoch": 1.0,
            "sessions": [{"pid": 1, "cwd": "/a", "tty": "", "tmux": null,
                "transcript": null, "session_id": "", "last_type": "",
                "last_ts": "", "idle_sec": null, "preview": "",
                "tool_running": false}]
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

    /// The resident path must produce the same shape of result as the
    /// one-shot, and survive a second poll on the already-running process
    /// (Linux only; needs a real python3).
    #[test]
    #[cfg(target_os = "linux")]
    fn resident_detector_round_trips() {
        let mut d = Detector::new();
        let s = Settings::default();
        let st = d.detect(&s).expect("first detect (spawns resident)");
        assert!(st.now_epoch > 0.0);
        let st2 = d.detect(&s).expect("second detect (resident process)");
        assert!(st2.now_epoch >= st.now_epoch);
    }
}
