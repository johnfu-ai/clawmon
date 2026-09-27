# cc-monitor — project notes for Claude Code

Claude Code WSL session monitor: a Tauri 2 Windows app that watches claude
processes inside WSL and auto-continues stuck sessions via `tmux send-keys`.
`core/` is platform-independent logic with the tests — including the WSL-side
Python scripts (`core/src/detect.py`, `core/src/usage.py`) and the driver
tests that run them; `src-tauri/` is the Windows shell; `ui/` is plain
static HTML/JS (no bundler).

## Building and verifying from the WSL session

This session runs in WSL; the user's Windows machine is reachable through
`/mnt/c` and WSL interop. Never build Tauri for Windows by full
cross-compilation from Linux — drive the Windows-native toolchain instead:

```bash
# 1. Pre-push validation (CI parity: ubuntu tests + windows clippy).
#    Plain `cargo check --target x86_64-pc-windows-msvc` is NOT enough —
#    it does not run clippy lints, and a lint has failed CI before.
cargo fmt --all --check
cargo clippy -p cc-monitor-core --all-targets -- -D warnings
cargo test -p cc-monitor-core
cargo test -p cc-monitor-core --test integration -- --ignored --test-threads=1
scripts/win-clippy.sh   # or: RC=.tools/bin/llvm-rc CC_x86_64_pc_windows_msvc=gcc \
                        #      cargo clippy --target x86_64-pc-windows-msvc -p cc-monitor --all-targets -- -D warnings

# 2. Windows-native exe build (proven path, ~4 min incremental).
scripts/build-local.sh
```

`scripts/build-local.sh` mirrors the repo into `C:\Users\<user>\cc-monitor-build`
(excluding `.git/`, `target/`, `ai-log.md` — the target dir is kept as the
incremental cache) and invokes the Windows-side `cargo.exe build --release`
through interop. Artifact: `C:\Users\<user>\cc-monitor-build\target\release\cc-monitor.exe`.

## Releases

Tag-driven only: push a `v*` tag and `.github/workflows/release.yml` builds
exe + installers on GitHub's Windows runners and attaches them to the release.
No local Windows build is ever involved in releases. The in-app auto-updater
was deliberately removed (2026-09-13, user decision) — do not reintroduce it.

## Design invariants

- The poll loop lives in the Rust backend, never in WebView JS: WebView2
  throttles hidden-window timers, and monitoring must survive tray/pet mode.
- Light semantics (2026-09-27, user decision): green means "no user
  action needed" — active, waiting on the API, a running tool, a subagent
  or a dynamic workflow (fresh writes under `subagents/agent-*.jsonl` or
  `subagents/workflows/<run>/`, which outrank a later local slash command),
  a thinking-only record, claude retrying an API error on its own, **or a
  stall whose auto-continue countdown is still running** (tmux, auto-continue
  on, attempts left). Blue means the turn completed (`turn_duration` /
  `stop_hook_summary`), the user interrupted, a local command already ran
  and no agent or workflow transcript is still fresh, or the process is
  idle at the prompt (a fresh session with no transcript, or a prompt past
  the timeout whose CPU is idle). Yellow means the user
  must provide input mid-flight (AskUserQuestion, ExitPlanMode, a paused
  goal) or we cannot tell (no transcript and CPU unknown, or a transcript
  we cannot trust). Red means only a person can fix it: a stall that
  auto-continue cannot take (not in tmux, auto-continue off, attempts used
  up), or a non-retryable API error (400/401/403/404). A usage-limit 429 is
  a stall, not blue — first send waits for the reset timestamp parsed from
  the error (`限额将在 … 重置` / `It will reset at …`), not a fresh
  `wait_secs` window. Extending green is legitimate only when the wait
  provably does not need the user. A 30s idle heuristic is not that proof
  — models think longer than that after a status sentence. Idle CPU is
  measured, not guessed: under 5 ticks/s of utime+stime (Claude Code
  2.1.280, 2026-09-27: prompt ≈ 0–2, a streaming turn ≈ 10–25). Unknown
  CPU (one-shot mode, first resident sample) trusts the transcript alone.
- Detections that cannot be trusted (no transcript and CPU unknown, or a
  transcript that predates the process) must stay yellow: the cost of a
  false red is pressing Enter in an unrelated terminal. An idle process is
  never auto-sent Enter.
- The resident detector (`detect.py --serve`) must degrade to the one-shot
  pipe on any failure — a broken resident process must never lose a poll.
- Manual and automatic sends share one invariant: book the attempt in the
  engine (under its lock) *before* the WSL round trip, or a poll firing
  mid-send presses Enter a second time.
- Rust→JS wire shapes are pinned by tests (`session_view_serializes_the_
  wire_contract`, `usage_info_serializes_the_wire_contract`): any field
  rename must be cross-checked against the ui/app.js readers named there.
- Language policy: webview text lives in ui/i18n.js (reason/pet keys), native
  text (notifications, tray) is formatted in the shell; there is no shared
  table between the two because no bundler bridges Rust and JS. Diagnostics
  from core (banner/toast errors) are Chinese-only by the same argument.
- Never hold two AppState mutexes at once (see the rule in src-tauri/src/
  lib.rs).
- `ai-log.md`: append one entry per user task per the global rules.
