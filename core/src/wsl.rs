use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// How long we wait for a WSL command before killing it (seconds).
const CMD_TIMEOUT_SECS: u64 = 25;

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

fn read_all(mut cmd: Command, stdin_data: Option<&str>) -> Result<String, String> {
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

    // Read output in a thread so we can enforce a timeout.
    let mut stdout = child.stdout.take().expect("stdout piped");
    let handle = thread::spawn(move || {
        use std::io::Read;
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });

    let deadline = Duration::from_secs(CMD_TIMEOUT_SECS);
    let waited = thread::spawn(move || child.wait());
    let start = std::time::Instant::now();
    loop {
        if waited.is_finished() {
            break;
        }
        if start.elapsed() > deadline {
            return Err(format!("命令超时（>{CMD_TIMEOUT_SECS}s），已放弃"));
        }
        thread::sleep(Duration::from_millis(50));
    }
    let status = waited
        .join()
        .map_err(|_| "等待进程失败".to_string())?
        .map_err(|e| format!("等待进程失败: {e}"))?;

    let output = handle.join().unwrap_or_default();
    if !status.success() {
        let msg = output.trim();
        // `tmux send-keys` prints errors on stderr (merged output read is
        // stdout only here) — report generic failure with code.
        return Err(format!(
            "命令退出码 {}{}",
            status.code().map(|c| c.to_string()).unwrap_or_default(),
            if msg.is_empty() {
                String::new()
            } else {
                format!(": {msg}")
            }
        ));
    }
    Ok(output)
}

/// Run a command inside WSL with no stdin.
pub fn run_wsl(distro: &str, args: &[&str]) -> Result<String, String> {
    read_all(build_command(distro, args), None)
}

/// Run a command inside WSL, feeding `input` on stdin.
/// Used for `python3 -` (script on stdin).
pub fn run_wsl_stdin(distro: &str, args: &[&str], input: &str) -> Result<String, String> {
    read_all(build_command(distro, args), Some(input))
}
