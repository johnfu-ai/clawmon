**English** | [中文](README.zh-CN.md)

# cc-monitor — Claude Code session monitor for WSL

A small Windows desktop app (Tauri 2) that watches Claude Code terminal
sessions running in **WSL**. It shows each session as a **red / yellow /
blue / green** light, and after a long wait caused by an **API usage
limit** (typically five hours) it sends a keystroke into the terminal so
the task continues.

## How it works

```
┌──────────── Windows ────────────┐      ┌──────── WSL ────────┐
│  Tauri 2 (Rust + WebView2)      │      │                      │
│  ┌──────────┐   wsl.exe   ┌─────┴────┐ │  ┌────────────────┐  │
│  │ poll     │ ─────────▶ │ python3  │─┼─▶ │ claude process │  │
│  │ thread   │ ◀───────── │ detector │ │  │ ~/.claude/     │  │
│  └────┬─────┘   JSON      └─────┬────┘ │  │  projects/*.jsonl│ │
│       │ events → light UI        │      │  └────────────────┘  │
│       │ tmux send-keys (wsl.exe)──────┼──▶ target pane         │
└─────────────────────────────────┘      └──────────────────────┘
```

The poll loop lives in a backend thread. The UI only subscribes to it.
A JavaScript timer inside the WebView is not enough: once the window is
minimized to the pet or the tray, WebView2 throttles timers in a hidden
window to about once a minute. Monitoring and auto-continue have to keep
running when the window is not in front.

- **Resident detector**: the detection script stays alive in WSL (`--serve`:
  one request line in, one JSON line out) instead of launching `wsl.exe`
  on every poll. If that process dies, times out, or returns something
  that cannot be parsed, the poll falls back to the one-shot pipe and
  starts a new resident process on the next round. A failure on the
  resident path never drops a poll.
- **Finding sessions**: scan `/proc` for claude processes (PID, working
  directory, TTY, whether they sit in a tmux pane) and map each one to
  `~/.claude/projects/<slug>/*.jsonl`. Concurrent claude processes in the
  same directory each pair with their own transcript (from `--session-id`
  / `--resume` on the command line, or by matching process start time to
  the first record timestamp). Closing a Windows Terminal tab does not
  kill node — Claude Code ignores SIGHUP — so the pts and the process
  can outlive the window. Those ghost sessions are hidden (pts is
  `(deleted)`, or the matching `wsl.exe` is left with a 0×0
  `PseudoConsoleWindow`). Sessions inside tmux stay listed: that is what
  tmux is for, and the window can be attached again.
- **Classifying state** (transcript tail, plus whether the process is
  actually busy):

  | Light | Condition | Meaning |
  |-------|-----------|---------|
  | 🟢 | Idle under the active threshold (default 2 minutes), or thinking-only | Working |
  | 🟢 | Pending tool is `Task` / `Agent` / `TaskOutput`, or a subagent / dynamic-workflow transcript is still fresh | Waiting on a subagent |
  | 🟢 | Pending tool is anything else except the two below | Tool running |
  | 🟢 | Last record is a user message, and the timeout has not elapsed | Waiting on the API (slow is not stuck) |
  | 🟢 | Claude wrote that it is “retrying” | API retry in progress |
  | 🟢 | Timed out, usage 429, or a retryable API error, and the session is in tmux with auto-continue on and attempts left | Counting down; cc-monitor will continue it |
  | 🔵 | Transcript ends with `turn_duration` / `stop_hook_summary` | Turn complete |
  | 🔵 | The user interrupted, or a local slash command already ran | Sitting at the prompt |
  | 🔵 | No transcript and the process is idle, or a timed-out prompt whose process is idle | Idle (not waiting on the API; Enter is not sent) |
  | 🟡 | Pending tool is `AskUserQuestion` or `ExitPlanMode` | Waiting for an answer or plan approval |
  | 🟡 | A goal is paused and the text asks you to send a message | Goal paused |
  | 🟡 | No transcript and CPU is unknown, or the paired transcript cannot be trusted | Unclear; no key is sent |
  | 🔴 | One of the stalls above, but the session is not in tmux, auto-continue is off, or attempts are used up | Needs you |
  | 🔴 | API error that cannot be retried (400/401/403/404) | Enter will not fix it |

  The lights answer “do you need to act?”. **Green means no**: that
  includes cc-monitor’s own countdown. **Blue** means the turn finished
  or the process is sitting at the prompt. **Yellow** means you must
  type, or the monitor cannot tell what the session is waiting on.
  **Red** means only a person can fix it. When the classification is
  uncertain, no key is sent. An idle process (resident detector: under
  5 ticks/s) never receives Enter.

  A dynamic workflow (Claude Code’s “Waiting for N dynamic workflow(s)
  to finish”) uses the same green light as a background subagent.
  Workflow agent records and stage journals live under
  `subagents/workflows/<run>/`. While those files are still fresh, a
  later local slash command (for example `/workflows`) does not
  reclassify the session as blue “idle”. After the files have been quiet
  longer than the subagent idle window (default 10 minutes), the session
  returns to blue from a completion marker or a local command.
- **Auto-continue**: a stall (timeout, usage limit, or a retryable API
  error) starts a timer, and the light stays green during it. A usage
  429 parses the reset time out of the error text (for example “限额将在
  2026-09-19 16:21:07 重置” / “It will reset at …”) and sends
  `tmux send-keys -t <pane> Enter` at that time. If the text has no
  reset time, it falls back to the configured wait (default **5 hours**).
  If the session is still stalled, it retries on an interval (default
  10 minutes, at most 3 times) and turns red when the attempts are used
  up. Every parameter is editable in Settings. Each attempt is booked
  at the moment it is scheduled, so two overlapping polls cannot send
  Enter twice. A stall outside tmux is red and says it cannot
  auto-continue because it is not in tmux.

> **Important**: only Claude Code running inside **tmux** can be
> controlled. Linux disables TIOCSTI, so injecting keystrokes into a pts
> from outside is not possible. Sessions outside tmux are monitor-only
> (labeled “monitor only” in the UI).

### Notifications

The engine edge-detects between polls. These transitions can raise a
Windows notification (each one has its own switch; “needs you”,
“auto-continue”, “recovered”, “needs input”, and “exited” default on;
“turn ended” defaults off) and an optional system beep:

| Event | Default | Meaning |
|-------|---------|---------|
| Session needs you | On | Entered red: auto-continue cannot take it, or the API error is not retryable |
| Auto-continue sent | On | Shows the keys and which attempt this was |
| Recovered | On | A stall or a red light ended on its own (usage window reset, and so on) |
| Turn ended | Off | Claude finished a turn |
| Needs your input | On | Waiting for an answer, plan approval, or a paused goal |
| Session exited | On | The claude process disappeared (finished or crashed) |

### Recommended WSL setup

```bash
tmux new -s work          # start a tmux session
cd ~/my-project
claude                    # start claude inside tmux
```

## Build (Windows)

Requirements:

- Rust (stable, via `rustup`)
- WebView2 Runtime (built into Windows 11; installed automatically on Windows 10 if missing)
- `python3` in the WSL distro (Ubuntu includes it) and `tmux` (`sudo apt install tmux`)

The frontend is static files. There is no Node build step. The Rust
toolchain is enough:

```powershell
cargo install tauri-cli --version "^2"
cargo tauri build          # produces the exe and msi
# development:
cargo tauri dev
```

Artifacts land in `target/release/` at the repo root (`cc-monitor.exe`
can be copied out and run).

### Drive the Windows build from WSL

With Rust installed on Windows (`%USERPROFILE%\.cargo`), a WSL session
can drive that native toolchain. This is a real Windows build, not a
cross-compile:

```bash
scripts/build-local.sh
```

The script mirrors the repo to `C:\Users\<you>\cc-monitor-build\`
(keeping `target\` as the incremental cache and excluding `.git`), runs
Windows `cargo.exe build --release`, and leaves the binary at
`C:\Users\<you>\cc-monitor-build\target\release\cc-monitor.exe`. A cold
build takes around fifteen minutes; later incremental builds take a
minute or two.

## Use

1. Start the app and leave it running. Minimizing it, or closing it to
   the system tray, still polls and auto-continues. Hovering the tray
   icon shows the red / yellow / blue / green counts.
2. Each claude process is one row: project name, light, status, idle
   time, this session’s token usage (`in x · cache x · out x · N req`,
   with a tooltip; summed from the transcript by message id, including
   subagents), a preview of the last output, and the tmux location.
3. A red light blinks and shows a countdown (“auto-continue in
   xx:xx:xx”). **Continue now** sends the key immediately.
4. **Desktop pet**: a coral crab in the Claude Code crab (Clawd) style
   sits at the bottom-right of the desktop, always on top, from startup.
   Expression plus a badge over its head follow the lights (green =
   blink, blue = a pleased blink and a blue count, yellow = dozing, red
   = shaking and a count badge, WSL disconnected = grey and asleep), and
   the claws wave in turn. Drag it anywhere. Click it to open the main
   window; the pet itself stays. Minimizing the main window only hides
   that window — the crab is the monitor. Clicks on the transparent
   corners pass through; only the crab catches the mouse.
5. **GLM plan usage** (status bar): reads `ANTHROPIC_BASE_URL` /
   `ANTHROPIC_AUTH_TOKEN` from `~/.claude/settings.json`. The token stays
   inside WSL and never crosses to Windows. Every five minutes it queries
   the plan quota API and shows the 5-hour and weekly quotas as
   “5h xx% · 7d xx%” (tooltip: credit breakdown, reset time, plan tier;
   70% turns yellow, 90% turns red). A failed query keeps the previous
   result and does not affect monitoring. Non-GLM endpoints hide the
   readout. It can be turned off in Settings.
6. ⚙ opens Settings (UI and notifications switch between 中文 and English):

| Setting | Default | Meaning |
|---------|---------|---------|
| Poll interval | 5 seconds | Time between detections |
| Active threshold | 120 seconds | How long without output counts as idle |
| Timeout threshold | 300 seconds | How long a response wait lasts before the light turns red |
| Auto-continue | On | Send the key when the red timer elapses |
| Wait | 5 hours | How long after turning red before sending |
| Continue keys | `Enter` | tmux key names, for example `C-c Enter` |
| Max attempts | 3 | Auto-send cap for one stall |
| Retry interval | 10 minutes | Gap after a send that did not recover the session |
| WSL distro | empty | Empty uses the default distro; or set `Ubuntu` and similar |
| GLM plan usage | On | Status-bar 5-hour / weekly quota (meaningful for GLM Coding Plan) |
| Close to tray | On | Closing the window minimizes to the tray and keeps monitoring; left-click the tray icon to restore, right-click to quit |
| Language | 中文 | UI and notification language: 中文 / English |
| Notifications ×5 | see table above | Needs you / auto-continue / recovered / turn ended / exited |
| Sound | On | Play a system beep with notifications |

## Layout

```
core/                 logic crate (no Tauri dependency; tested on its own)
  src/detector.rs      WSL detect calls (resident process + one-shot fallback) and JSON parsing
  src/detect.py        embedded WSL detector (one-shot and --serve)
  src/engine.rs        state machine (lights, countdown, auto-continue, event edges)
  src/settings.rs      settings persistence and validation
  src/usage.rs         GLM plan quota query and JSON parsing
  src/usage.py         embedded WSL quota script (reads ~/.claude/settings.json; the token stays in WSL)
  src/wsl.rs           wsl.exe wrapper (Windows) / direct calls (Linux tests) + resident child
src-tauri/
  src/lib.rs           poll thread, usage thread, notifications, Tauri commands
scripts/
  build-local.sh       WSL-driven native Windows build (see Build)
  win-clippy.sh        clippy from WSL, matching the CI Windows job
ui/                   static frontend (HTML/CSS/JS, no build step)
  i18n.js              zh/en dictionary (zh is the default UI language; shared by the main window and the pet)
  pet.html/pet.js      desktop pet (transparent always-on-top window)
```

## Known limits

- Concurrent claude processes in one directory each pair with their own
  transcript. A file that finished writing before the process started is
  not paired (it would show up as a stale “transcript not ready”).
  Without `--session-id` / `--resume`, a transcript that is still being
  written but whose beginning is much older than this process can lose
  out to a later transcript in the same directory.
- A running tool is green (“tool running”), not a cue to send Enter.
  `ExitPlanMode` and `AskUserQuestion` are yellow. An ordinary permission
  prompt still looks like a tool that is running; check the terminal.
  An idle process is not sent Enter just because the transcript stopped
  on an old question.
- Auto-continue only sends keys. If claude has already exited, the keys
  do nothing — the exit itself is detected and can notify (“session
  ended”).
- Detection uses the resident process by default (one stdin/stdout round
  trip per poll, milliseconds). If that process fails, that poll falls
  back to a one-shot `wsl.exe` call (about 0.1–1 second). If WSL itself
  is not running, the first launch still pays a full WSL startup.

## Development

The logic lives in the `core` crate and does not depend on Tauri:

```bash
cargo test -p cc-monitor-core                          # unit tests
cargo test -p cc-monitor-core --test integration -- --ignored   # end to end (needs tmux)
cargo clippy -p cc-monitor-core --all-targets          # lints
python3 -m py_compile core/src/detect.py core/src/usage.py   # WSL scripts
```

CI runs all of the above, including the end-to-end test. See
`.github/workflows/ci.yml`.

To check the Windows shell from Linux/WSL, use clippy rather than
`cargo check`. Check does not run lints; a `needless_borrow` has failed
the Windows CI job before:

```bash
rustup target add x86_64-pc-windows-msvc
scripts/win-clippy.sh    # same clippy -D warnings as the CI Windows job
```

(`win-clippy.sh` writes a stub to `.tools/bin/llvm-rc` so `tauri-winres`
can find a resource compiler. A real Windows build does not need that
stub.)

## License

This project is licensed under the MIT License.

Copyright (c) 2026 johnfu-ai

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
