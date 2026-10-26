#!/bin/sh
# Run one test of a built test binary N times, each in its own process (like nextest).
# Usage: rs-stress.sh <count> <binary> <exact test name> <log>
set -u
count=$1; binary=$2; test=$3; log=$4
pass=0; fail=0
: > "$log"
i=1
while [ "$i" -le "$count" ]; do
  if out=$("$binary" --exact "$test" --test-threads 1 2>&1) && printf '%s' "$out" | grep -q "1 passed"; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    printf '=== run %s failed\n%s\n' "$i" "$out" >> "$log"
  fi
  i=$((i + 1))
done
echo "runs=$count pass=$pass fail=$fail" | tee -a "$log"
