#!/bin/sh
# Ordinary build load: rebuild studio-runtime-services with cargo -j 4 until the stop file exists.
# Usage: build-load.sh <stop file>
set -u
stop=$1
export PATH="/opt/homebrew/bin:/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="$HOME/Projects/codex-agents-flaky/target-load"
cd "$HOME/Projects/codex-agents-flaky" || exit 1
n=0
while [ ! -e "$stop" ]; do
  free=$(df -g / | awk 'NR==2 {print $4}')
  if [ "$free" -lt 15 ]; then echo "stop: free disk ${free} GB"; break; fi
  touch workspaces/runtime/packages/runtime-services/src/lib.rs
  cargo build -j 4 --locked -p studio-runtime-services >/dev/null 2>&1
  n=$((n + 1))
  echo "build $n done $(date +%H:%M:%S)"
done
