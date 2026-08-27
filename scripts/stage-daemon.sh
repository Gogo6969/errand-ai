#!/usr/bin/env bash
# Put a freshly built daemon where the bundler will find it.
#
# The bundler copies app/binaries/errandd-<triple> into the app by target
# triple. Nothing rebuilds that file, so it is whatever was last put there, and
# a bundle built from a stale one ships a daemon older than the database it
# opens. The symptom is not obvious: the app installs its LaunchAgent, the
# daemon starts, and dies with "migration 8 was previously applied but is
# missing in the resolved migrations", which reads like a corrupt database and
# is nothing of the kind.
#
# CI never hit it, because CI stages the file fresh in the job above. Only a
# person building locally, from a working copy where the file is nine hours
# old, gets an app that cannot start.
#
# So this is part of the build now, rather than a step somebody remembers.
set -euo pipefail
cd "$(dirname "$0")/.."

TRIPLE=$(rustc -vV | sed -n 's/host: //p')
cargo build --release -p errand-runner --bin errandd

mkdir -p app/binaries
rm -f "app/binaries/errandd-$TRIPLE"
cp target/release/errandd "app/binaries/errandd-$TRIPLE"
chmod 755 "app/binaries/errandd-$TRIPLE"

# The check that would have caught it. cargo can report success and leave the
# last executable in place when clippy or test has won the fingerprint.
NEWEST=$(find core runner -name '*.rs' -o -name '*.sql' | xargs ls -t | head -1)
if [ -n "$NEWEST" ] && [ "target/release/errandd" -ot "$NEWEST" ]; then
  echo "Refusing to stage: target/release/errandd is older than $NEWEST." >&2
  echo "  cargo reported success but did not rewrite the binary." >&2
  echo "  Run: cargo clean -p errand-runner && $0" >&2
  exit 1
fi
echo "staged app/binaries/errandd-$TRIPLE ($(date -r "target/release/errandd" '+%H:%M'))"
