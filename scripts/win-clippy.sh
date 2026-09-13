#!/usr/bin/env bash
# Windows-target clippy from inside WSL — the same lint pass the CI windows
# job runs. `cargo check --target ...` compiles but does NOT run clippy, and
# a needless-borrow lint has failed CI before; run this before pushing.
set -euo pipefail
cd "$(dirname "$0")/.."

# Fake llvm-rc for cross clippy (no real linking happens). Self-healing on a
# fresh clone: the stub is (re)created here rather than tracked as a binary
# under the git-ignored .tools/ tree, so the documented pre-push flow works
# from any checkout.
RC_DEFAULT="$PWD/.tools/bin/llvm-rc"
if [ -z "${RC:-}" ] && [ ! -x "$RC_DEFAULT" ]; then
    mkdir -p "$(dirname "$RC_DEFAULT")"
    cat > "$RC_DEFAULT" <<'STUB'
#!/bin/bash
# Fake llvm-rc for cross `cargo clippy` (no real linking happens in check).
# Usage shape: rc-stub /fo <out.res> /c 65001 -- <input.rc>
prev=""
for a in "$@"; do
  if [ "$prev" = "/fo" ]; then : > "$a"; fi
  prev="$a"
done
exit 0
STUB
    chmod +x "$RC_DEFAULT"
fi
export RC="${RC:-$RC_DEFAULT}"

export CC_x86_64_pc_windows_msvc="${CC_x86_64_pc_windows_msvc:-gcc}"
exec cargo clippy --target x86_64-pc-windows-msvc -p clawmon --all-targets -- -D warnings
