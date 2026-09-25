use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// How long we wait for a WSL command before killing it (seconds).
pub const CMD_TIMEOUT_SECS: u64 = 25;
/// How long we keep waiting for the output pipes *after* the process is gone.
const DRAIN_GRACE_SECS: u64 = 2;
/// How often we re-check whether the child has exited.
const POLL_INTERVAL_MS: u64 = 50;

fn build_command(distro: &str, args: &[&str]) -> Command {
    let mut cmd;
    #[cfg(windows)]
    {
        cmd = Command::new("wsl.exe");
        if !distro.is_empty() {
            cmd.arg("-d").arg(distro);
        }
        for a in args {
            cmd.arg(a);
        }
        // CREATE_NO_WINDOW: don't flash a console window on every poll.
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        // Non-Windows (dev/testing inside WSL): run the tool directly.
        let _ = distro;
        cmd = Command::new(args[0]);
        for a in &args[1..] {
            cmd.arg(a);
        }
    }
    cmd
}

/// Drain a piped stream on its own thread, handing the decoded text back over
/// a channel. Reading both pipes concurrently is what keeps the child from
/// blocking on a full 64 KiB pipe buffer — with stderr piped and never read,
/// every command that writes more than a few lines would hang until timeout.
///
/// The channel (rather than a `JoinHandle`) means we never block forever on a
/// thread whose reader has no EOF yet, e.g. because a grandchild inherited the
/// pipe.
fn drain<R: Read + Send + 'static>(mut reader: R) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        // WSL output is UTF-8 in practice, but a mangled byte must not turn
        // the whole poll into a silent empty result.
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

fn collect(rx: Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(DRAIN_GRACE_SECS))
        .unwrap_or_default()
}

fn read_all(
    mut cmd: Command,
    stdin_data: Option<&str>,
    timeout: Duration,
) -> Result<String, String> {
    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("启动命令失败: {e}"))?;

    if let Some(data) = stdin_data {
        use std::io::Write;
        let mut stdin = child.stdin.take().ok_or_else(|| "无 stdin".to_string())?;
        stdin
            .write_all(data.as_bytes())
            .map_err(|e| format!("写入 stdin 失败: {e}"))?;
        // dropping stdin closes the pipe
    }

    let stdout = drain(child.stdout.take().expect("stdout piped"));
    let stderr = drain(child.stderr.take().expect("stderr piped"));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(|e| format!("等待进程失败: {e}"))? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                // Kill *and* reap: a wsl.exe left running here would pile up
                // one process per poll for as long as WSL stays wedged.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("命令超时（>{}s），已终止", timeout.as_secs_f64()));
            }
            None => thread::sleep(Duration::from_millis(POLL_INTERVAL_MS)),
        }
    };

    let output = collect(stdout);
    let errors = collect(stderr);
    if !status.success() {
        let detail = if errors.trim().is_empty() {
            output.trim()
        } else {
            errors.trim()
        };
        return Err(format!(
            "命令退出码 {}{}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    Ok(output)
}

/// Run a command inside WSL with no stdin.
pub fn run_wsl(distro: &str, args: &[&str]) -> Result<String, String> {
    read_all(
        build_command(distro, args),
        None,
        Duration::from_secs(CMD_TIMEOUT_SECS),
    )
}

/// Send tmux key names to a pane. Keys travel as argv entries, never through
/// a shell. The shell's send paths and the integration test share this so
/// the command shape has exactly one home.
pub fn tmux_send_keys(distro: &str, pane: &str, keys: &[&str]) -> Result<(), String> {
    let mut args: Vec<&str> = vec!["tmux", "send-keys", "-t", pane];
    args.extend_from_slice(keys);
    run_wsl(distro, &args).map(|_| ())
}

/// Create a detached tmux session for a task launch (FR10.2). `-c` makes
/// tmux itself change into the working directory; the command string is
/// interpreted by the login shell inside WSL — the same trust boundary as
/// the user typing it. Name and cwd travel as single argv entries.
pub fn tmux_new_session(distro: &str, name: &str, cwd: &str, command: &str) -> Result<(), String> {
    run_wsl(
        distro,
        &["tmux", "new-session", "-d", "-s", name, "-c", cwd, command],
    )
    .map(|_| ())
}

/// Kill a task's tmux session (the task "stop" button). Killing a session
/// that is already gone is fine — callers treat that as success.
pub fn tmux_kill_session(distro: &str, name: &str) -> Result<(), String> {
    run_wsl(distro, &["tmux", "kill-session", "-t", name]).map(|_| ())
}

/// Open a visible terminal attached to a tmux session (FR10.4): Windows
/// Terminal when available, else a windowed wsl.exe console. The terminal
/// is its own process — clawmon exiting must never close it — so this only
/// spawns and never waits, and it must NOT use CREATE_NO_WINDOW.
pub fn open_terminal(distro: &str, session: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

        let attach = |cmd: &mut Command| {
            cmd.arg("wsl.exe");
            if !distro.is_empty() {
                cmd.arg("-d").arg(distro);
            }
            cmd.arg("--").args(["tmux", "attach", "-t", session]);
        };

        let mut wt = Command::new("wt.exe");
        attach(&mut wt);
        if wt
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return Ok(());
        }

        let mut wsl = Command::new("wsl.exe");
        attach(&mut wsl);
        wsl.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开终端失败: {e}"))
    }
    #[cfg(not(windows))]
    {
        // dev/test inside WSL: nothing sensible to open, and monitoring does
        // not depend on this button — the detached session is already visible
        // to the detector. Report success so the UI does not nag.
        let _ = (distro, session);
        Ok(())
    }
}

/// Run a command inside WSL, feeding `input` on stdin.
/// Used for `python3 -` (script on stdin).
pub fn run_wsl_stdin(distro: &str, args: &[&str], input: &str) -> Result<String, String> {
    read_all(
        build_command(distro, args),
        Some(input),
        Duration::from_secs(CMD_TIMEOUT_SECS),
    )
}

/// A long-lived child inside WSL speaking one-line-in / one-line-out over
/// its stdio. One resident `python3` for every poll instead of a fresh
/// `wsl.exe` boot per poll — each of those costs a Windows process spawn
/// plus a WSL round trip (0.1–1 s).
pub struct Persistent {
    child: Child,
    stdin: ChildStdin,
    /// stdout lines, handed over by a reader thread so `request` can time
    /// out without blocking on a pipe that will never see another byte
    lines: Receiver<String>,
}

impl Persistent {
    /// Spawn `args` inside the distro with piped stdio.
    pub fn spawn(distro: &str, args: &[&str]) -> Result<Persistent, String> {
        let mut cmd = build_command(distro, args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("启动常驻进程失败: {e}"))?;
        let stdin = child.stdin.take().ok_or("无 stdin")?;
        let stdout = child.stdout.take().ok_or("无 stdout")?;
        let stderr = child.stderr.take().ok_or("无 stderr")?;

        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break, // EOF or broken pipe
                    Ok(_) => {
                        if tx.send(line.clone()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        // stderr must never fill its pipe and block the child: swallow it
        thread::spawn(move || {
            let mut sink = stderr;
            let mut buf = Vec::new();
            let _ = sink.read_to_end(&mut buf);
        });

        Ok(Persistent {
            child,
            stdin,
            lines,
        })
    }

    /// Send one request line, wait for one response line. Any failure marks
    /// the child as gone so the next call respawns it.
    pub fn request(&mut self, line: &str, timeout: Duration) -> Result<String, String> {
        use std::io::Write;
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("写入常驻进程失败: {e}"))?;
        match self.lines.recv_timeout(timeout) {
            Ok(l) => Ok(l.trim_end_matches(['\n', '\r']).to_string()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.kill();
                Err("常驻进程响应超时".into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("常驻进程已退出".into()),
        }
    }

    /// Still running? A dead child is respawned by the caller.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill and reap, so a wedged WSL cannot leak child processes.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Persistent {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    fn run(args: &[&str], timeout_secs: u64) -> Result<String, String> {
        read_all(
            build_command("", args),
            None,
            Duration::from_secs(timeout_secs),
        )
    }

    #[test]
    fn captures_stdout() {
        let out = run(&["sh", "-c", "echo hello"], 5).unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[test]
    fn failure_reports_stderr() {
        let err = run(&["sh", "-c", "echo boom >&2; exit 3"], 5).unwrap_err();
        assert!(err.contains('3'), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    /// A child that fills the stderr pipe must still finish: stderr used to be
    /// piped but never read, so this deadlocked until the timeout fired.
    #[test]
    fn does_not_deadlock_on_large_stderr() {
        let out = run(
            &[
                "sh",
                "-c",
                "dd if=/dev/zero bs=1024 count=256 2>/dev/null | tr '\\0' 'x' >&2; echo done",
            ],
            20,
        )
        .expect("must not time out");
        assert_eq!(out.trim(), "done");
    }

    /// On timeout the child is killed, not left behind. The script writes its
    /// pid so we can check afterwards that it is really gone.
    #[test]
    fn timeout_kills_the_child() {
        let pidfile = std::env::temp_dir().join("clawmon-wsl-timeout.pid");
        let _ = std::fs::remove_file(&pidfile);
        let script = format!("echo $$ > {} ; exec sleep 60", pidfile.display());
        let start = Instant::now();
        let err = run(&["sh", "-c", &script], 1).unwrap_err();
        assert!(err.contains("超时"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timeout not enforced"
        );

        let pid = std::fs::read_to_string(&pidfile)
            .expect("child never wrote its pid")
            .trim()
            .to_string();
        // give the kernel a moment to reap
        thread::sleep(Duration::from_millis(200));
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "child {pid} survived the timeout"
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    /// Task launch primitives (FR10.2): a detached session appears, and
    /// killing it makes it vanish. `has-session` for the negative check on
    /// purpose: killing the last session takes the whole server down with
    /// it, and `list-sessions` would then fail with "no server running" —
    /// exactly the state we want to assert as "gone". Linux-only like the
    /// other live tests — needs a real tmux.
    #[test]
    #[cfg(all(test, target_os = "linux"))]
    fn task_session_lifecycle() {
        let name = "clawmon-wsl-test-task";
        let _ = run(&["tmux", "kill-session", "-t", name], 5);
        tmux_new_session("", name, "/tmp", "sleep 60").expect("new-session");
        let list = run(&["tmux", "list-sessions", "-F", "#{session_name}"], 5).unwrap();
        assert!(
            list.lines().any(|l| l.trim() == name),
            "task session must be listed: {list}"
        );
        tmux_kill_session("", name).expect("kill-session");
        assert!(
            run(&["tmux", "has-session", "-t", name], 5).is_err(),
            "task session must be gone"
        );
    }

    #[test]
    fn persistent_child_answers_requests() {
        let mut p = Persistent::spawn(
            "",
            &[
                "sh",
                "-c",
                "while read -r line; do echo \"got:$line\"; done",
            ],
        )
        .unwrap();
        assert!(p.is_alive());
        assert_eq!(p.request("a", Duration::from_secs(5)).unwrap(), "got:a");
        assert_eq!(p.request("b", Duration::from_secs(5)).unwrap(), "got:b");
        p.kill();
        assert!(!p.is_alive());
    }

    /// No reply within the timeout → the request fails and the child is
    /// killed, never left wedged for the next poll to trip over.
    #[test]
    fn persistent_request_times_out_and_kills() {
        let mut p = Persistent::spawn("", &["sh", "-c", "cat > /dev/null"]).unwrap();
        let err = p.request("x", Duration::from_secs(1)).unwrap_err();
        assert!(err.contains("超时"), "{err}");
        assert!(!p.is_alive());
    }
}
