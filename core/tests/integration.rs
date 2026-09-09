//! End-to-end test for the full monitoring loop. Requires a live WSL/Linux
//! environment with tmux — run with `cargo test -p clawmon-core --test
//! integration -- --ignored`.

use clawmon_core::{detect, wsl::run_wsl, Engine, SessionState, SessionView, Settings};
use std::path::PathBuf;
use std::process::Command;

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
        let _ = std::fs::remove_dir_all(
            self.home
                .join(".claude")
                .join("projects")
                .join(&self.slug),
        );
    }
}

fn run(settings: &Settings) -> (Vec<SessionView>, Vec<i32>, clawmon_core::RawStatus) {
    let snap = detect(settings).expect("detect");
    let mut e = Engine::new(settings.clone());
    let (views, due) = e.update(snap.clone());
    (views, due, snap)
}

#[test]
#[ignore = "requires live WSL/Linux + tmux"]
fn detects_blocked_session_and_auto_continues() {
    let fx = Fixture::setup();

    let mut st = Settings::default();
    st.wait_secs = 0; // fire immediately for the test
    st.blocked_after_secs = 60;
    st.idle_green_secs = 10;
    st.resume_keys = "CLAWMON-FIRED".to_string();

    let (views, _, _) = run(&st);
    let v = views
        .iter()
        .find(|v| v.cwd == fx.cwd.to_str().unwrap())
        .expect("fixture session not detected");
    assert_eq!(v.state, SessionState::Red, "stale user entry must be red");
    assert_eq!(v.label, "疑似 API 超时");
    assert!(v.controllable, "tmux session should be controllable");
    assert!(v.remaining_sec.is_some());

    // full pass with the engine, mirroring what the tauri command does
    let snap = detect(&st).unwrap();
    let mut e = Engine::new(st.clone());
    let (_, due) = e.update(snap);
    assert!(!due.is_empty(), "auto-continue must fire with wait_secs=0");
    for pid in due {
        let pane = e.get_session(pid).unwrap().tmux.as_ref().unwrap().pane.clone();
        let keys: Vec<&str> = st.resume_keys.split_whitespace().collect();
        let mut args: Vec<&str> = vec!["tmux", "send-keys", "-t", &pane];
        args.extend(keys.iter().copied());
        run_wsl("", &args).expect("send-keys");
        e.record_send(pid);
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
