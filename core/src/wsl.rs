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
