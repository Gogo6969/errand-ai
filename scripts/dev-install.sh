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

# Sign with a stable identity when one exists.
#
# Ad-hoc signing (`--sign -`) produces a different code identity on every
# build, so macOS treats each rebuild as a different program and the keychain
# items created by the last one no longer match. That shows up as a permission
# prompt nobody can see, and it wasted real time before this was understood.
IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
  | grep -m1 -E "Apple Development|Developer ID Application" \
  | sed -E 's/.*"(.*)"/\1/')
if [ -n "$IDENTITY" ]; then
  echo "Signing $DEST as: $IDENTITY"
  codesign --force --sign "$IDENTITY" "$DEST"
else
  echo "Signing $DEST ad-hoc (no developer certificate found)"
  echo "  Note: ad-hoc signatures change on every build, so you will be asked"
  echo "  for keychain permission again after each rebuild."
  codesign --force --sign - "$DEST"
fi
codesign --verify "$DEST" && echo "  signature ok"

# Stage the same binary where the bundler looks for it, so that a local
# `cargo tauri build` produces the app CI produces rather than one with no
# daemon inside it. tauri matches externalBin sources by target triple and
# strips the suffix again on the way in, which is how it arrives at
# Contents/MacOS/errandd, the path the LaunchAgent above is written against.
TRIPLE=$(rustc -vV | sed -n 's/host: //p')
mkdir -p app/binaries
rm -f "app/binaries/errandd-$TRIPLE"
cp "$SRC" "app/binaries/errandd-$TRIPLE"
chmod 755 "app/binaries/errandd-$TRIPLE"
echo "Staged app/binaries/errandd-$TRIPLE for cargo tauri build"

echo "Installing LaunchAgent"
"$DEST" install "$DEST"

sleep 3
if curl -fsS --max-time 5 "http://127.0.0.1:4477/v1/health" >/dev/null 2>&1; then
  echo "Runner is up: $(curl -fsS http://127.0.0.1:4477/v1/health)"
else
  echo "Runner did not answer on 127.0.0.1:4477. Run: $DEST doctor"
  exit 1
fi
