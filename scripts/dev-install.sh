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
# Release by default, and that is not just about speed.
#
# A release build keeps its secrets in the macOS keychain and is signed with a
# stable identity, so macOS asks permission once and the answer sticks. A debug
# build deliberately keeps its secrets in a file instead, because cargo relinks
# it on every compile and macOS treats each relink as a different program; it
# would ask permission again after every single build, and the habit of clicking
# Allow without reading is worse than anything the prompt was protecting.
#
# So the daemon you actually live with should be a release build. Pass "debug"
# if you want the other one for a moment.
PROFILE="${1:-release}"

echo "Building ($PROFILE)"
if [ "$PROFILE" = "release" ]; then
  # The daemon first, and staged before anything else is built.
  #
  # The app crate's build script requires app/binaries/errandd-<triple> to
  # already exist, and that file is a build artefact nobody checks in. So in a
  # fresh clone or a new worktree, a plain workspace build fails on the app
  # before it ever reaches the staging step further down that would have made
  # the file. The daemon is what this script is here to install anyway, so it
  # is built and staged first and the rest of the workspace follows.
  ./scripts/stage-daemon.sh
  cargo build --release -q
  SRC="target/release/errandd"
else
  cargo build -q
  SRC="target/debug/errandd"
fi

# Check the build actually produced a binary newer than the code.
#
# cargo will report success and leave a stale executable behind: clippy and
# test share this target directory and their artifacts can win the fingerprint,
# so `cargo build` prints nothing and target/release/errandd stays as it was.
# Installing that is a genuinely confusing failure -- an older binary knows
# fewer migrations, so the daemon dies with "migration 5 was previously applied
# but is missing", which reads like a corrupt database rather than a stale copy.
# app/src, not app/src-tauri: the tauri crate lives at app/ in this repo, and
# with pipefail a find over a directory that is not there fails the assignment
# and set -e ends the script on the spot, silently, having printed nothing
# since the last echo. Two people have now watched this exit 1 with no reason
# given, so the path is the one that exists and the failure is not swallowed.
NEWEST=$(find core runner app/src -name '*.rs' -o -name '*.sql' \
  | xargs ls -t | head -1)
if [ -n "$NEWEST" ] && [ "$SRC" -ot "$NEWEST" ]; then
  echo "Refusing to install: $SRC is older than $NEWEST." >&2
  echo "  cargo reported success but did not rewrite the binary." >&2
  echo "  Run: cargo clean -p errand-runner && $0 $PROFILE" >&2
  exit 1
fi
echo "  $SRC is newer than the code it was built from"

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
