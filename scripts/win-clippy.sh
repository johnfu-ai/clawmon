#!/usr/bin/env bash
# Windows-target clippy from inside WSL — the same lint pass the CI windows
# job runs. `cargo check --target ...` compiles but does NOT run clippy, and
# a needless-borrow lint has failed CI before; run this before pushing.
set -euo pipefail
cd "$(dirname "$0")/.."
export RC="${RC:-$PWD/.tools/bin/llvm-rc}"
export CC_x86_64_pc_windows_msvc="${CC_x86_64_pc_windows_msvc:-gcc}"
exec cargo clippy --target x86_64-pc-windows-msvc -p clawmon --all-targets -- -D warnings
