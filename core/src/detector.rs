use crate::settings::Settings;
use crate::wsl::{run_wsl_stdin, Persistent, CMD_TIMEOUT_SECS};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// The embedded WSL-side detection script (single source of truth; it is
/// piped to `wsl.exe -e python3 -` at runtime). The script lives beside its
/// Rust adapter and is driven by the tests below — the two halves of the
/// JSON contract change together.
pub const DETECT_SCRIPT: &str = include_str!("detect.py");

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
#[serde(rename_all = "camelCase")]
pub struct RawUsage {
    #[serde(default)]
    pub input: u64,
    // the aliases are load-bearing: detect.py emits snake_case while the
    // webview consumes camelCase — without them the inbound pipe breaks.
    // The serialized shape is pinned by engine::session_view_serializes_
    // the_wire_contract.
    #[serde(default, alias = "cache_read")]
    pub cache_read: u64,
    #[serde(default, alias = "cache_creation")]
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

    /// The CLAUDE.md invariant: any resident failure must degrade to the
    /// one-shot pipe for that poll. A broken resident is simulated by
    /// swapping in a child that always answers with unparseable garbage —
    /// exactly what a crashed script looks like from here. This also pins
    /// the subtle half of the trick: the garbage must fail to parse as a
    /// `RawStatus` (a struct-level serde default would silently turn it
    /// into "zero sessions").
    #[test]
    #[cfg(target_os = "linux")]
    fn broken_resident_degrades_to_one_shot() {
        let mut d = Detector::new();
        let s = Settings::default();
        let _ = d
            .detect(&s)
            .expect("first detect spawns a healthy resident");
        d.child = Some(
            Persistent::spawn(
                "",
                &["sh", "-c", "while read -r line; do echo not-json; done"],
            )
            .expect("saboteur spawns"),
        );
        let st = d.detect(&s).expect("poll must still succeed via one-shot");
        assert!(st.now_epoch > 0.0);
        // the broken child is gone and the next poll respawns cleanly
        let st2 = d.detect(&s).expect("third detect (respawned resident)");
        assert!(st2.now_epoch >= st.now_epoch);
    }

    /// Drives the real pairing heuristics over a fixture projects tree:
    /// `--session-id` evidence wins exactly, an older decoy file stays
    /// unclaimed, and a process whose project dir has nothing falls through
    /// to the cwd tail-match scan (Linux only; needs a real python3).
    #[test]
    #[cfg(target_os = "linux")]
    fn pairing_respects_session_id_and_tail_match() {
        const DRIVER: &str = r#"
import importlib.util, json, os, sys, time

detect_path, root = sys.argv[1], sys.argv[2]
spec = importlib.util.spec_from_file_location("clawmon_detect", detect_path)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

projects = os.path.join(root, "projects")
mod.CLAUDE_DIR = projects

xdir = os.path.join(projects, "-tmp-x")
os.makedirs(xdir)
sid = "aaaa0000-0000-0000-0000-000000000001"
sid_file = os.path.join(xdir, sid + ".jsonl")
old_file = os.path.join(xdir, "bbbb0000-0000-0000-0000-000000000002.jsonl")
with open(sid_file, "w") as f:
    f.write('{"type":"user","timestamp":"2026-09-13T00:00:00Z","sessionId":"s1"}\n')
with open(old_file, "w") as f:
    f.write('{"type":"user","timestamp":"2026-01-01T00:00:00Z","sessionId":"old"}\n')
os.utime(old_file, (0, 0))  # the decoy is ancient

# tail-match candidate: lives in another project dir, recent, its last
# recorded cwd points at /tmp/nowhere
odir = os.path.join(projects, "-tmp-other")
os.makedirs(odir)
tail_file = os.path.join(odir, "cccc0000-0000-0000-0000-000000000003.jsonl")
with open(tail_file, "w") as f:
    f.write('{"type":"user","cwd":"/tmp/nowhere","timestamp":"2026-09-13T00:00:00Z","sessionId":"s2"}\n')
now = time.time()
os.utime(tail_file, (now, now))

now_epoch = 1789000000.0
procs = [
    {"pid": 101, "cmd": ["claude", "--session-id", sid],
     "cwd": "/tmp/x", "tty": "", "tmux": None, "start": now_epoch},
    {"pid": 102, "cmd": ["claude"],
     "cwd": "/tmp/nowhere", "tty": "", "tmux": None, "start": now_epoch},
]
mapping = mod.assign_transcripts(procs)
print(json.dumps({str(k): os.path.basename(v) if v else None
                  for k, v in mapping.items()}))
"#;
        use std::fs;
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("detect.py");
        let driver = tmp.path().join("driver.py");
        fs::write(&script, DETECT_SCRIPT).unwrap();
        fs::write(&driver, DRIVER).unwrap();
        let out = Command::new("python3")
            .args([
                driver.to_str().unwrap(),
                script.to_str().unwrap(),
                tmp.path().to_str().unwrap(),
            ])
            .output()
            .expect("python3");
        assert!(
            out.status.success(),
            "driver failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mapping: std::collections::HashMap<String, Option<String>> =
            serde_json::from_slice(&out.stdout).expect("driver printed the mapping");
        assert_eq!(
            mapping.get("101").unwrap().as_deref(),
            Some("aaaa0000-0000-0000-0000-000000000001.jsonl"),
            "--session-id evidence must win exactly"
        );
        assert_eq!(
            mapping.get("102").unwrap().as_deref(),
            Some("cccc0000-0000-0000-0000-000000000003.jsonl"),
            "no local candidates -> the cwd tail-match scan applies"
        );
        assert!(
            !mapping
                .values()
                .any(|v| v.as_deref() == Some("bbbb0000-0000-0000-0000-000000000002.jsonl")),
            "the old decoy must stay unclaimed"
        );
    }

    /// Regression: `/clear` retires the transcript a process was born
    /// with and starts a newer one in the same project dir — the birth
    /// match alone kept pairing the process with the closed file forever
    /// (idle time frozen at the last pre-clear entry, so a hard-working
    /// session showed "waiting", then "possible API timeout"). Drives the
    /// real `assign_transcripts`: a cleared process follows the new file,
    /// an idle one keeps its own, and a file still being written after a
    /// stranger was born is not a clear (Linux only; needs a real
    /// python3).
    #[test]
    #[cfg(target_os = "linux")]
    fn pairing_follows_clear_not_idle() {
        const DRIVER: &str = r#"
import importlib.util, json, os, sys

detect_path, root = sys.argv[1], sys.argv[2]
spec = importlib.util.spec_from_file_location("clawmon_detect", detect_path)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

projects = os.path.join(root, "projects")
mod.CLAUDE_DIR = projects

ts = mod.parse_ts
closeout = {"type": "cost-state", "sessionId": "x"}

def mk(slug, name, first_iso, close, mtime):
    d = os.path.join(projects, slug)
    os.makedirs(d, exist_ok=True)
    p = os.path.join(d, name)
    with open(p, "w") as f:
        f.write(json.dumps({"type": "user", "timestamp": first_iso,
                            "sessionId": name[:-6]}) + "\n")
        if close:
            f.write(json.dumps(close) + "\n")
    os.utime(p, (mtime, mtime))
    return p

# the bug: the file born with the process ends with the untimestamped
# close-out record /clear appends, and a newer file exists
clr = "-tmp-clr"
old0 = mk(clr, "old00000-0000-0000-0000-000000000001.jsonl",
          "2026-09-13T13:00:00Z", closeout, ts("2026-09-13T13:54:00Z"))
new0 = mk(clr, "new00000-0000-0000-0000-000000000002.jsonl",
          "2026-09-13T13:54:00Z", None, ts("2026-09-13T14:04:00Z"))

# idle at the prompt: the last entry is timestamped, so a newer
# unclaimed file (a dead neighbour's leftover) must not be adopted
idle = "-tmp-idle"
keep0 = mk(idle, "keep0000-0000-0000-0000-000000000003.jsonl",
           "2026-09-13T12:00:00Z", None, ts("2026-09-13T12:56:00Z"))
mk(idle, "leftovr0-0000-0000-0000-000000000004.jsonl",
   "2026-09-13T13:30:00Z", None, ts("2026-09-13T13:40:00Z"))

# close-out marker, but the file was still written long after the newer
# file was born — not a clear of this process
hot = "-tmp-hot"
work0 = mk(hot, "work0000-0000-0000-0000-000000000005.jsonl",
           "2026-09-13T13:00:00Z", closeout, ts("2026-09-13T14:04:00Z"))
mk(hot, "late0000-0000-0000-0000-000000000006.jsonl",
   "2026-09-13T13:50:00Z", None, ts("2026-09-13T14:04:00Z"))

procs = [
    {"pid": 201, "cmd": ["claude"], "cwd": "/tmp/clr",
     "tty": "", "tmux": None, "start": ts("2026-09-13T13:00:05Z")},
    {"pid": 202, "cmd": ["claude"], "cwd": "/tmp/idle",
     "tty": "", "tmux": None, "start": ts("2026-09-13T12:00:05Z")},
    {"pid": 203, "cmd": ["claude"], "cwd": "/tmp/hot",
     "tty": "", "tmux": None, "start": ts("2026-09-13T13:00:05Z")},
]
mapping = mod.assign_transcripts(procs)
print(json.dumps({str(k): os.path.basename(v) if v else None
                  for k, v in mapping.items()}))
"#;
        use std::fs;
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("detect.py");
        let driver = tmp.path().join("driver.py");
        fs::write(&script, DETECT_SCRIPT).unwrap();
        fs::write(&driver, DRIVER).unwrap();
        let out = Command::new("python3")
            .args([
                driver.to_str().unwrap(),
                script.to_str().unwrap(),
                tmp.path().to_str().unwrap(),
            ])
            .output()
            .expect("python3");
        assert!(
            out.status.success(),
            "driver failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mapping: std::collections::HashMap<String, Option<String>> =
            serde_json::from_slice(&out.stdout).expect("driver printed the mapping");
        assert_eq!(
            mapping.get("201").unwrap().as_deref(),
            Some("new00000-0000-0000-0000-000000000002.jsonl"),
            "a cleared process must follow the post-clear transcript"
        );
        assert_eq!(
            mapping.get("202").unwrap().as_deref(),
            Some("keep0000-0000-0000-0000-000000000003.jsonl"),
            "an idle session keeps its transcript"
        );
        assert_eq!(
            mapping.get("203").unwrap().as_deref(),
            Some("work0000-0000-0000-0000-000000000005.jsonl"),
            "a file still being written after the stranger was born is not retired"
        );
        assert!(
            !mapping
                .values()
                .any(|v| v.as_deref() == Some("old00000-0000-0000-0000-000000000001.jsonl")),
            "the retired transcript must stay burned"
        );
    }

    /// Regression: child claude processes are not sessions. Plugins and
    /// agent SDKs spawn claude binaries that descend from the real session
    /// with a pipe on stdin — one terminal used to show up as many rows.
    /// Spawns three fake processes and drives the real `collect()`:
    /// a pts-stdin one (kept), a pipe-stdin one (dropped), and a
    /// claude-child-of-claude (dropped even though it inherits the pts).
    /// (Linux only; needs a real /proc and `script` for the pty.)
    #[test]
    #[cfg(target_os = "linux")]
    fn child_and_pipe_stdin_claude_processes_are_not_sessions() {
        const DRIVER: &str = r#"
import importlib.util, json, os, subprocess, sys, time

detect_path, work = sys.argv[1], sys.argv[2]
spec = importlib.util.spec_from_file_location("clawmon_detect", detect_path)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

# The fixtures must not be descendants of whatever runs this driver — on a
# developer box that is often a claude process itself, whose ancestry would
# (correctly) filter them. `setsid --fork` re-parents each fixture to init
# and the shim records its own pid, which survives the exec into "claude".
def detached(sh_body, stdin=subprocess.DEVNULL):
    pf = os.path.join(work, "pid%d" % detached.n)
    detached.n += 1
    # bash, not sh: the fixtures rely on `exec -a`, which dash's exec lacks
    subprocess.Popen(
        ["setsid", "--fork", "bash", "-c", "echo $$ > %s; %s" % (pf, sh_body)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        stdin=stdin)
    return pf
detached.n = 0

def wait_pid(pf):
    for _ in range(40):
        try:
            with open(pf) as f:
                return int(f.read().strip())
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("pidfile never appeared: %s" % pf)

# a real-looking session: claude with a terminal on stdin
kept_script = detached(
    'exec script -qec "exec -a claude sleep 60" /dev/null')
# same argv[0], but stdin is a pipe handed over by the spawner
piped_pf = detached("exec -a claude sleep 60", stdin=subprocess.PIPE)
# a claude descending from another claude: the middle bash forks the inner
# one and stays alive (`& wait`) — a plain `bash -c "exec …"` would just
# replace itself and the pair would never exist as two processes
nested = ('exec script -qec "exec -a claude bash -c '
          '\'exec -a claude sleep 60 & wait\'" /dev/null')
nested_script = detached(nested)

time.sleep(0.5)
scan = mod.collect()

def children(pid):
    out = []
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open("/proc/%s/status" % d) as f:
                for line in f:
                    if line.startswith("PPid:"):
                        if int(line.split()[1]) == pid:
                            out.append(int(d))
                        break
        except OSError:
            pass
    return out

kept_pid = wait_pid(kept_script)
nested_pid = wait_pid(nested_script)
piped_pid = wait_pid(piped_pf)
kept_claude = children(kept_pid)[:1]      # `script` wraps the claude
outer = children(nested_pid)[:1]          # the outer claude
inner = children(outer[0])[:1] if outer else []  # the inner claude
pids = {s["pid"] for s in scan["sessions"]}

alive = lambda p: os.path.exists("/proc/%d" % p)
# capture before the cleanup kills, so "not listed" is never vacuous
was_alive = [alive(p) for p in [piped_pid] + kept_claude + outer + inner]

for p in [kept_pid, nested_pid, piped_pid] + kept_claude + outer + inner:
    try:
        os.kill(p, 9)
    except OSError:
        pass

print(json.dumps({
    "layout": {"kept": kept_claude, "outer": outer, "inner": inner},
    "alive": was_alive,
    "kept": kept_claude[0] in pids if kept_claude else None,
    "piped": piped_pid in pids,
    "outer_claude": outer[0] in pids if outer else None,
    "inner_claude": inner[0] in pids if inner else None,
}))
"#;
        use std::fs;
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("detect.py");
        let driver = tmp.path().join("driver.py");
        fs::write(&script, DETECT_SCRIPT).unwrap();
        fs::write(&driver, DRIVER).unwrap();
        let out = Command::new("python3")
            .args([
                driver.to_str().unwrap(),
                script.to_str().unwrap(),
                tmp.path().to_str().unwrap(),
            ])
            .output()
            .expect("python3");
        assert!(
            out.status.success(),
            "driver failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let result: serde_json::Value =
            serde_json::from_slice(&out.stdout).expect("driver printed its verdict");
        let layout = &result["layout"];
        for (key, why) in [
            ("kept", "pts fixture"),
            ("outer", "nested outer"),
            ("inner", "nested inner"),
        ] {
            assert_eq!(
                layout[key].as_array().map(|a| a.len()),
                Some(1),
                "the {why} fixture did not spawn: {result}"
            );
        }
        assert!(
            result["alive"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v == &serde_json::Value::Bool(true)),
            "a fixture died before the scan — verdicts would be vacuous: {result}"
        );
        assert_eq!(result["kept"], true, "a pts-stdin claude must be listed");
        assert_eq!(
            result["piped"], false,
            "a pipe-stdin claude (agent child) must not be listed"
        );
        assert_eq!(
            result["inner_claude"], false,
            "a claude descending from another claude must not be listed \
             even with a pts on stdin"
        );
        assert_eq!(
            result["outer_claude"], true,
            "the outer claude of the pair is a normal session"
        );
    }
}
