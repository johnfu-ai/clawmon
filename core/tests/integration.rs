//! End-to-end test for the full monitoring loop. Requires a live WSL/Linux
//! environment with tmux — run with `cargo test -p clawmon-core --test
//! integration -- --ignored`.

use clawmon_core::{detect, wsl::run_wsl, Engine, SessionState, SessionView, Settings};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Refresh a file's mtime.
///
/// The detector only trusts a transcript that was written at or after the
/// process it is paired with started — that is what stops a stale session
/// file from being reported as a stuck session. A fixture that lays the
/// record down before spawning the process therefore has to claim it
/// explicitly.
fn touch(path: &Path) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now()).unwrap();
}

struct Fixture {
    home: PathBuf,
    slug: String,
    cwd: PathBuf,
    tmux: String,
}

impl Fixture {
    fn setup() -> Self {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let cwd = std::env::temp_dir().join("clawmon-it");
        std::fs::create_dir_all(&cwd).unwrap();
        // transcript dir derived from cwd (/tmp/clawmon-it → -tmp-clawmon-it)
        let slug = {
            let s = cwd.to_str().unwrap();
            let mut out = String::new();
            for c in s.chars() {
                out.push(if c.is_ascii_alphanumeric() { c } else { '-' });
            }
            out
        };
        let proj = home.join(".claude").join("projects").join(&slug);
        std::fs::create_dir_all(&proj).unwrap();
        // a transcript whose last entry is a *user* message from long ago:
        // claude is stuck waiting for an assistant response that never comes
        let transcript = proj.join("00000000-dead-beef-0000-000000000000.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","sessionId":"itest","cwd":"/tmp/clawmon-it","message":{"role":"assistant","content":[{"type":"text","text":"前一轮回复"}]}}"#, "\n",
                r#"{"type":"user","timestamp":"2026-01-01T00:10:00Z","sessionId":"itest","cwd":"/tmp/clawmon-it","message":{"role":"user","content":"继续干活"}}"#, "\n",
            ),
        )
        .unwrap();

        // a fake "claude" process inside tmux, cwd = our fixture dir
        let tmux = "clawmonit".to_string();
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &tmux])
            .status();
        let cmd = format!(
            "bash -c 'cd {} && exec -a claude sleep 90'",
            cwd.to_str().unwrap()
        );
        Command::new("tmux")
            .args(["new-session", "-d", "-s", &tmux, &cmd])
            .status()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        touch(&transcript);

        Self {
            home,
            slug,
            cwd,
            tmux,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.tmux])
            .status();
        let _ =
            std::fs::remove_dir_all(self.home.join(".claude").join("projects").join(&self.slug));
    }
}

fn run(settings: &Settings) -> (Vec<SessionView>, Vec<i32>, clawmon_core::RawStatus) {
    let snap = detect(settings).expect("detect");
    let mut e = Engine::new(settings.clone());
    let (views, due, _) = e.update(snap.clone());
    (views, due, snap)
}

#[test]
#[ignore = "requires live WSL/Linux + tmux"]
fn detects_blocked_session_and_auto_continues() {
    let fx = Fixture::setup();

    let st = Settings {
        wait_secs: 0, // fire immediately for the test
        blocked_after_secs: 60,
        idle_green_secs: 10,
        resume_keys: "CLAWMON-FIRED".to_string(),
        ..Default::default()
    };

    let (views, _, _) = run(&st);
    let v = views
        .iter()
        .find(|v| v.cwd == fx.cwd.to_str().unwrap())
        .expect("fixture session not detected");
    assert_eq!(v.state, SessionState::Red, "stale user entry must be red");
    assert_eq!(v.label, "疑似 API 超时");
    assert!(v.controllable, "tmux session should be controllable");
    assert!(v.remaining_sec.is_some());

    // full pass with the engine, mirroring what the poll loop does
    let snap = detect(&st).unwrap();
    let mut e = Engine::new(st.clone());
    let (_, due, _) = e.update(snap);
    assert!(!due.is_empty(), "auto-continue must fire with wait_secs=0");
    for pid in due {
        let pane = e
            .get_session(pid)
            .unwrap()
            .tmux
            .as_ref()
            .unwrap()
            .pane
            .clone();
        let keys: Vec<&str> = st.resume_keys.split_whitespace().collect();
        let mut args: Vec<&str> = vec!["tmux", "send-keys", "-t", &pane];
        args.extend(keys.iter().copied());
        run_wsl("", &args).expect("send-keys");
        // no record_send here: update() already counted the attempt
    }

    std::thread::sleep(std::time::Duration::from_millis(500));
    let cap = Command::new("tmux")
        .args(["capture-pane", "-t", &fx.tmux, "-p"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&cap.stdout);
    assert!(
        text.contains("CLAWMON-FIRED"),
        "keys did not reach the pane: {text}"
    );
}

/// Two claude processes in the same directory must map to two distinct
/// transcript files (per-session mapping), and only the stale one goes red.
#[test]
#[ignore = "requires live WSL/Linux + tmux"]
fn concurrent_sessions_get_distinct_transcripts() {
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let cwd = std::env::temp_dir().join("clawmon-it2");
    let slug = "-tmp-clawmon-it2";
    let proj = home.join(".claude").join("projects").join(slug);
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    // clean slate: any files left by an earlier run break the pairing
    for f in std::fs::read_dir(&proj).unwrap().flatten() {
        let _ = std::fs::remove_file(f.path());
    }

    let tmux = "clawmonit2";
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", tmux])
        .status();

    // file B: session started "now" — its first entry matches proc 1 start
    let t0 = iso_now_plus(0);
    std::fs::write(
        proj.join("00000000-0000-0000-0000-00000000000b.jsonl"),
        format!(
            concat!(
                r#"{{"type":"user","timestamp":"{}","sessionId":"sess-b","cwd":"/tmp/clawmon-it2","message":{{"role":"user","content":"hi"}}}}"#, "\n",
                r#"{{"type":"assistant","timestamp":"{}","sessionId":"sess-b","cwd":"/tmp/clawmon-it2","message":{{"role":"assistant","content":[{{"type":"text","text":"done"}}]}}}}"#, "\n",
            ),
            t0, t0,
        ),
    )
    .unwrap();
    Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            tmux,
            &format!("bash -c 'cd {} && exec -a claude sleep 90'", cwd.display()),
        ])
        .status()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1200));

    // file A: first entry matches proc 2 start (~now+1.2s) but its last
    // entry is an old user message → this session is blocked
    let t1 = iso_now_plus(0);
    let old = iso_now_plus(-400);
    std::fs::write(
        proj.join("00000000-0000-0000-0000-00000000000a.jsonl"),
        format!(
            concat!(
                r#"{{"type":"user","timestamp":"{}","sessionId":"sess-a","cwd":"/tmp/clawmon-it2","message":{{"role":"user","content":"start"}}}}"#, "\n",
                r#"{{"type":"user","timestamp":"{}","sessionId":"sess-a","cwd":"/tmp/clawmon-it2","message":{{"role":"user","content":"continue"}}}}"#, "\n",
            ),
            t1, old,
        ),
    )
    .unwrap();
    Command::new("tmux")
        .args([
            "new-window",
            "-d",
            "-t",
            tmux,
            &format!("bash -c 'cd {} && exec -a claude sleep 90'", cwd.display()),
        ])
        .status()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    // both records were laid down before their process started
    touch(&proj.join("00000000-0000-0000-0000-00000000000a.jsonl"));
    touch(&proj.join("00000000-0000-0000-0000-00000000000b.jsonl"));

    let st = Settings::default();
    let snap = detect(&st).expect("detect");
    let ours: Vec<_> = snap
        .sessions
        .iter()
        .filter(|s| s.cwd == "/tmp/clawmon-it2")
        .collect();
    assert_eq!(ours.len(), 2, "both fake claude processes must be found");

    let mut e = Engine::new(st);
    let (views, _, _) = e.update(snap);
    let views: Vec<_> = views
        .into_iter()
        .filter(|v| v.cwd == "/tmp/clawmon-it2")
        .collect();
    assert_eq!(views.len(), 2);

    // distinct transcript mapping
    let ids: std::collections::HashSet<&str> =
        views.iter().map(|v| v.session_id.as_str()).collect();
    assert_eq!(
        ids.len(),
        2,
        "sessions must not share a transcript: {ids:?}"
    );

    // exactly the stale one is red
    let red: Vec<_> = views
        .iter()
        .filter(|v| v.state == SessionState::Red)
        .collect();
    assert_eq!(red.len(), 1, "exactly one session must be red");
    assert_eq!(red[0].session_id, "sess-a");
    assert_eq!(red[0].label, "疑似 API 超时");

    let green: Vec<_> = views
        .iter()
        .filter(|v| v.state != SessionState::Red)
        .collect();
    assert_eq!(green[0].session_id, "sess-b");

    let _ = Command::new("tmux")
        .args(["kill-session", "-t", tmux])
        .status();
    let _ = std::fs::remove_dir_all(&proj);
    let _ = std::fs::remove_dir_all(&cwd);
}

/// Minimal RFC3339 timestamp helper (avoids a chrono dependency in tests).
fn iso_now_plus(secs: i64) -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + secs;
    // days since epoch → civil date (Howard Hinnant's algorithm)
    let days = t.div_euclid(86400);
    let secs_of_day = t.rem_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3600,
        secs_of_day % 3600 / 60,
        secs_of_day % 60
    )
}
