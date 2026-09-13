#!/usr/bin/env bash
# Build the Windows exe from inside WSL using the Windows-native toolchain.
# Mirrors the repo into C:\Users\<user>\clawmon-build (keeping target\ as the
# incremental cache) and runs the Windows cargo through WSL interop.
set -euo pipefail
SRC="$(cd "$(dirname "$0")/.." && pwd)"

WINUSER="$(/mnt/c/Windows/System32/cmd.exe /c "echo %USERNAME%" 2>/dev/null | tr -d '\r')"
if [ -z "$WINUSER" ]; then
    echo "error: cannot reach Windows (no WSL interop?)" >&2
    exit 1
fi
DEST="/mnt/c/Users/$WINUSER/clawmon-build"
CARGO="/mnt/c/Users/$WINUSER/.cargo/bin/cargo.exe"
if [ ! -x "$CARGO" ]; then
    echo "error: Windows cargo not found at $CARGO" >&2
    exit 1
fi

echo "syncing $SRC -> $DEST"
mkdir -p "$DEST"
rsync -a --delete \
    --exclude '.git/' \
    --exclude 'target/' \
    --exclude 'ai-log.md' \
    "$SRC/" "$DEST/"

cd "$DEST"
"$CARGO" build --release
echo
echo "exe: C:\\Users\\$WINUSER\\clawmon-build\\target\\release\\clawmon.exe"
