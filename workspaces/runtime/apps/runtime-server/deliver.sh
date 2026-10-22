#!/usr/bin/env bash
# Deliver one commit to the live server of this Linux host.
#
# The release is a source snapshot of the commit (git archive) with its
# renderer, Claude bridge dependencies and release binary, so the binary finds
# its install root beside itself. A `current` link selects the live release;
# a systemd drop-in starts the unit from it. A release that does not stay
# healthy is rolled back to the previous one.
set -euo pipefail

# The systemd user unit of the live server.
readonly UNIT=codex-studio
# TCP port of the live server.
readonly PORT=4620
# Seconds to wait for the first HTTP 200 after a restart.
readonly START_TIMEOUT_SECONDS=120
# Seconds the unit must then stay active without a restart.
readonly STABLE_SECONDS=30
# Release directories kept on disk, the current and previous ones included.
readonly KEPT_RELEASES=3

repository=$(git rev-parse --show-toplevel)
commit=$(git -C "$repository" rev-parse --verify "${1:-HEAD}^{commit}")
root=${CODEX_STUDIO_SERVER_ROOT:-$HOME/.local/share/codex-studio/server}
release=$root/releases/$commit
binary=bin/studio-runtime-server
dropin=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$UNIT.service.d/zz-delivery.conf

# One build directory for every release, so a delivery reuses earlier builds.
export CARGO_TARGET_DIR=$root/target

mkdir -p "$root/releases"
exec 9>"$root/deliver.lock"
flock -n 9 || { echo "another delivery is running" >&2; exit 1; }

if [ ! -x "$release/$binary" ]; then
  staging=$release.partial
  rm -rf "$staging"
  mkdir -p "$staging/bin"
  git -C "$repository" archive "$commit" | tar -x -C "$staging"
  pnpm --dir "$staging" install --frozen-lockfile \
    --filter codex-agents-web... --filter studio-claude-bridge...
  pnpm --dir "$staging" --filter codex-agents-web run build
  cargo build --release --locked \
    --manifest-path "$staging/Cargo.toml" -p studio-runtime-server
  cp "$root/target/release/studio-runtime-server" "$staging/$binary"
  mv "$staging" "$release"
fi

healthy() {
  local deadline=$((SECONDS + START_TIMEOUT_SECONDS)) restarts pid
  until curl -fsS -o /dev/null "http://127.0.0.1:$PORT/"; do
    [ "$SECONDS" -lt "$deadline" ] || return 1
    sleep 1
  done
  restarts=$(systemctl --user show -p NRestarts --value "$UNIT")
  sleep "$STABLE_SECONDS"
  pid=$(systemctl --user show -p MainPID --value "$UNIT")
  systemctl --user is-active --quiet "$UNIT" &&
    [ "$(systemctl --user show -p NRestarts --value "$UNIT")" = "$restarts" ] &&
    [ "$(readlink -f "/proc/$pid/exe")" = "$(readlink -f "$1/$binary")" ] &&
    curl -fsS -o /dev/null "http://127.0.0.1:$PORT/"
}

start() {
  ln -sfn "$1" "$root/current.next"
  mv -T "$root/current.next" "$root/current"
  mkdir -p "$(dirname "$dropin")"
  printf '[Service]\nExecStart=\nExecStart=%s/current/%s --port %s\n' \
    "$root" "$binary" "$PORT" >"$dropin"
  systemctl --user daemon-reload
  systemctl --user restart "$UNIT"
}

previous=$(readlink "$root/current" || true)
start "$release"
if ! healthy "$release"; then
  echo "release $commit is unhealthy; rolling back" >&2
  if [ -n "$previous" ]; then
    start "$previous"
  else
    rm -f "$root/current" "$dropin"
    systemctl --user daemon-reload
    systemctl --user restart "$UNIT"
  fi
  exit 1
fi

ls -1dt "$root/releases"/*/ | tail -n +$((KEPT_RELEASES + 1)) | while read -r old; do
  old=${old%/}
  [ "$old" = "$release" ] || [ "$old" = "$previous" ] || rm -rf "$old"
done
echo "delivered $commit"
