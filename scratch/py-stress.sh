#!/bin/sh
# Run one guest test N times, each in its own interpreter and temporary root.
# Usage: py-stress.sh <count> <test id> <log>
set -u
export PATH="/opt/homebrew/bin:$PATH"
count=$1; test=$2; log=$3
guest="$HOME/Projects/codex-agents-flaky/workspaces/runtime/apps/vm-guest"
pass=0; fail=0
: > "$log"
i=1
while [ "$i" -le "$count" ]; do
  temporary=$(mktemp -d /private/tmp/gXXXXXX)
  if out=$(cd "$guest" && TMPDIR="$temporary" CODEX_AGENTS_PYTHON=/nonexistent \
      python3 -B -m unittest "$test" 2>&1); then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    printf '=== run %s failed\n%s\n' "$i" "$out" >> "$log"
  fi
  rm -rf "$temporary"
  i=$((i + 1))
done
echo "runs=$count pass=$pass fail=$fail" | tee -a "$log"
