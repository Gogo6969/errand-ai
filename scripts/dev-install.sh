#!/usr/bin/env bash
# Install the runner as a LaunchAgent for development.
#
# Never point the agent at target/debug directly. cargo rewrites that file on
# every build, and macOS will deadlock a launchd-spawned process in dyld while
# amfid tries to validate a signature that changed underneath it: the process
# sits in __open on its own binary forever, alive but never reaching main, with
# no log and nothing listening. Confirmed the hard way.
#
# So: build, copy to a stable path, sign it there, and point the agent at the
# copy. This also mirrors the shipping shape, where the agent points into
# /Applications/Errand-AI.app and only changes during an update handover.

set -euo pipefail
cd "$(dirname "$0")/.."

DEST_DIR="$HOME/Library/Application Support/com.errandai.app/bin"
DEST="$DEST_DIR/errandd"
PROFILE="${1:-debug}"

echo "Building ($PROFILE)"
if [ "$PROFILE" = "release" ]; then
  cargo build --release -q
  SRC="target/release/errandd"
else
  cargo build -q
  SRC="target/debug/errandd"
fi

echo "Stopping any running runner"
launchctl bootout "gui/$(id -u)/com.errandai.runner" 2>/dev/null || true
pkill -9 -f 'errandd --launchd' 2>/dev/null || true
sleep 1

mkdir -p "$DEST_DIR"
# Remove rather than overwrite: replacing the bytes of a binary that the kernel
# has a cached signature for is what causes the dyld stall.
rm -f "$DEST"
cp "$SRC" "$DEST"
chmod 755 "$DEST"

echo "Signing $DEST"
codesign --force --sign - "$DEST"
codesign --verify "$DEST" && echo "  signature ok"

echo "Installing LaunchAgent"
"$DEST" install "$DEST"

sleep 3
if curl -fsS --max-time 5 "http://127.0.0.1:4477/v1/health" >/dev/null 2>&1; then
  echo "Runner is up: $(curl -fsS http://127.0.0.1:4477/v1/health)"
else
  echo "Runner did not answer on 127.0.0.1:4477. Run: $DEST doctor"
  exit 1
fi
