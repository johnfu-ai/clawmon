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
    /// tokens consumed by this session so far (None before the first scan)
    #[serde(default)]
    pub usage: Option<RawUsage>,
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

/// Per-session token usage aggregated by the detector: assistant
/// `message.usage` deduped by message id (unique ids ≈ API requests),
/// subagent transcripts included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawUsage {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_creation: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub requests: u32,
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
                "usage": {
                    "input": 1100000, "cache_read": 1200000,
                    "cache_creation": 5000, "output": 89000, "requests": 42
                },
                "tool_running": false,
                "transcript_live": true
            }]
        }"#;
        let st: RawStatus = serde_json::from_str(raw).unwrap();
        assert_eq!(st.sessions.len(), 1);
        assert_eq!(st.sessions[0].tmux.as_ref().unwrap().pane, "%0");
        assert_eq!(st.sessions[0].idle_sec, Some(5));
        let u = st.sessions[0].usage.expect("usage present");
        assert_eq!(u.input, 1_100_000);
        assert_eq!(u.cache_read, 1_200_000);
        assert_eq!(u.cache_creation, 5_000);
        assert_eq!(u.output, 89_000);
        assert_eq!(u.requests, 42);
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
        assert!(st.sessions[0].usage.is_none());
    }

    /// `usage` is optional too — and every field inside it defaults, so a
    /// leaner payload still parses.
    #[test]
    fn missing_usage_defaults_to_none() {
        let raw = r#"{"now":"x","now_epoch":1.0,"sessions":[{"pid":1,"cwd":"/a",
            "tty":"","tmux":null,"transcript":null,"session_id":"",
            "last_type":"","last_ts":"","idle_sec":null,"preview":"",
            "usage":{}}]}"#;
        let st: RawStatus = serde_json::from_str(raw).unwrap();
        let u = st.sessions[0].usage.expect("usage present");
        assert_eq!(u, RawUsage::default());
    }

    /// Drives the real detect.py scanner in one python process: duplicate
    /// message ids dedupe (last record wins), appended bytes fold in
    /// incrementally, a truncated file resets the cache, and subagent
    /// transcripts count toward the session (Linux only; like the live
    /// tests it needs a real python3).
    #[test]
    #[cfg(target_os = "linux")]
    fn transcript_usage_dedupes_and_increments() {
        const DRIVER: &str = r#"
import importlib.util, json, os, sys

detect_path, work = sys.argv[1], sys.argv[2]
spec = importlib.util.spec_from_file_location("clawmon_detect", detect_path)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

main = os.path.join(work, "sess.jsonl")

def rec(mid, inp, out=0):
    return json.dumps({
        "type": "assistant",
        "message": {"id": mid, "role": "assistant",
                    "usage": {"input_tokens": inp, "output_tokens": out,
                              "cache_read_input_tokens": 0,
                              "cache_creation_input_tokens": 0}},
        "timestamp": "2026-09-13T00:00:00Z",
    })

results = []
with open(main, "w") as f:
    f.write(rec("msg_a", 100) + "\n")
    f.write(rec("msg_a", 300) + "\n")   # duplicate id: last one wins
    f.write(rec("msg_b", 50, 7) + "\n")
    f.write(rec(None, 999) + "\n")      # no id: skipped, not summed
results.append(mod.transcript_usage(main))

with open(main, "a") as f:              # appended bytes fold in
    f.write(rec("msg_c", 10, 1) + "\n")
results.append(mod.transcript_usage(main))

with open(main, "w") as f:              # rewritten shorter: reset + rescan
    f.write(rec("msg_b", 50, 7) + "\n")
results.append(mod.transcript_usage(main))

sub = os.path.join(work, "sess", "subagents")   # subagent usage folds in
os.makedirs(sub)
with open(os.path.join(sub, "agent-x.jsonl"), "w") as f:
    f.write(rec("msg_s", 10, 2) + "\n")
results.append(mod.transcript_usage(main))

print(json.dumps(results))
"#;
        use std::fs;
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("detect.py");
        let driver = tmp.path().join("driver.py");
        let work = tmp.path().join("work");
        fs::write(&script, DETECT_SCRIPT).unwrap();
        fs::write(&driver, DRIVER).unwrap();
        fs::create_dir(&work).unwrap();
        let out = Command::new("python3")
            .args([
                driver.to_str().unwrap(),
                script.to_str().unwrap(),
                work.to_str().unwrap(),
            ])
            .output()
            .expect("python3");
        assert!(
            out.status.success(),
            "driver failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let results: Vec<Option<RawUsage>> =
            serde_json::from_slice(&out.stdout).expect("driver printed usage JSON");
        let u = |i: usize| results[i].expect("usage present");
        // msg_a(300) + msg_b(50); the id-less record never counts
        assert_eq!((u(0).input, u(0).output, u(0).requests), (350, 7, 2));
        // + msg_c(10/1)
        assert_eq!((u(1).input, u(1).output, u(1).requests), (360, 8, 3));
        // file rewritten to just msg_b: cache reset, totals rebuilt
        assert_eq!((u(2).input, u(2).output, u(2).requests), (50, 7, 1));
        // + subagent msg_s(10/2)
        assert_eq!((u(3).input, u(3).output, u(3).requests), (60, 9, 2));
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
