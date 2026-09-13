# clawmon — project notes for Claude Code

Claude Code WSL session monitor: a Tauri 2 Windows app that watches claude
processes inside WSL and auto-continues stuck sessions via `tmux send-keys`.
`core/` is platform-independent logic with the tests; `src-tauri/` is the
Windows shell; `ui/` is plain static HTML/JS (no bundler).

## Building and verifying from the WSL session

This session runs in WSL; the user's Windows machine is reachable through
`/mnt/c` and WSL interop. Never build Tauri for Windows by full
cross-compilation from Linux — drive the Windows-native toolchain instead:

```bash
# 1. Pre-push validation (CI parity: ubuntu tests + windows clippy).
#    Plain `cargo check --target x86_64-pc-windows-msvc` is NOT enough —
#    it does not run clippy lints, and a lint has failed CI before.
cargo fmt --all --check
cargo clippy -p clawmon-core --all-targets -- -D warnings
cargo test -p clawmon-core
cargo test -p clawmon-core --test integration -- --ignored --test-threads=1
scripts/win-clippy.sh   # or: RC=.tools/bin/llvm-rc CC_x86_64_pc_windows_msvc=gcc \
                        #      cargo clippy --target x86_64-pc-windows-msvc -p clawmon --all-targets -- -D warnings

# 2. Windows-native exe build (proven path, ~4 min incremental).
scripts/build-local.sh
```

`scripts/build-local.sh` mirrors the repo into `C:\Users\<user>\clawmon-build`
(excluding `.git/`, `target/`, `ai-log.md` — the target dir is kept as the
incremental cache) and invokes the Windows-side `cargo.exe build --release`
through interop. Artifact: `C:\Users\<user>\clawmon-build\target\release\clawmon.exe`.

## Releases

Tag-driven only: push a `v*` tag and `.github/workflows/release.yml` builds
exe + installers on GitHub's Windows runners and attaches them to the release.
No local Windows build is ever involved in releases. The in-app auto-updater
was deliberately removed (2026-09-13, user decision) — do not reintroduce it.

## Design invariants

- The poll loop lives in the Rust backend, never in WebView JS: WebView2
  throttles hidden-window timers, and monitoring must survive tray/pet mode.
- Detections that cannot be trusted (no/old transcript) must stay yellow:
  the cost of a false red is pressing Enter in an unrelated terminal.
- The resident detector (`detect.py --serve`) must degrade to the one-shot
  pipe on any failure — a broken resident process must never lose a poll.
- `ai-log.md`: append one entry per user task per the global rules.
