#!/usr/bin/env bash
# Deliver one commit to the live server of this Linux host.
#
# The release is a source snapshot of the commit (git archive) with its
# renderer, Claude bridge dependencies and release binary, so the binary finds
# its install root beside itself. A `current` link selects the live release;
# a systemd drop-in starts the unit from it. A release that does not stay
# healthy is rolled back to the previous one.
set -euo pipefail

# Service layout has one owner, shared with the Rust operator commands.
layout_values=$(node -e 'const c=JSON.parse(require("node:fs").readFileSync(process.argv[1], "utf8")); for(const k of ["unitName","port","binaryRelative","rootRelative","currentRelative"]) console.log(c[k])' "$(dirname "$0")/service-layout.json")
mapfile -t layout <<<"$layout_values"
readonly UNIT=${layout[0]}
readonly PORT=${layout[1]}
# Seconds to wait for the first HTTP 200 after a restart.
readonly START_TIMEOUT_SECONDS=120
# Seconds the unit must then stay active without a restart.
readonly STABLE_SECONDS=30
# Release directories kept on disk, the current and previous ones included.
readonly KEPT_RELEASES=3

repository=$(git rev-parse --show-toplevel)
commit=$(git -C "$repository" rev-parse --verify "${1:-HEAD}^{commit}")
# A service restart kills descendants of the old server, including its command
# monitors. Hand off before taking the lock or building so the entire delivery
# belongs to an independent service. Only disposable fixtures may bypass this.
if [ "${CODEX_STUDIO_DELIVER_IN_UNIT:-}" != 1 ] &&
   [ "${CODEX_STUDIO_DELIVER_TEST_IN_PROCESS:-}" != 1 ]; then
  command -v systemd-run >/dev/null || {
    echo "systemd-run is required to deliver outside the server's cgroup" >&2
    exit 1
  }
  command -v journalctl >/dev/null || {
    echo "journalctl is required to read delivery output" >&2
    exit 1
  }
  # One owner for both the transient service identity and its journal lookup.
  # The caller PID distinguishes repeated deliveries of the same commit.
  readonly DELIVERY_UNIT=codex-studio-deliver-${commit:0:12}-$$.service
  script=$(cd -- "$(dirname -- "$0")" && pwd)/${0##*/}
  environment=(--setenv=CODEX_STUDIO_DELIVER_IN_UNIT=1)
  # A user manager does not inherit the invoking shell's toolchain and paths.
  # Pass variable names rather than exposing their values in the argument list.
  for name in HOME PATH XDG_CONFIG_HOME XDG_CACHE_HOME XDG_DATA_HOME \
    XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS CODEX_STUDIO_SERVER_ROOT \
    CARGO_HOME RUSTUP_HOME CARGO_BUILD_JOBS RUSTFLAGS CARGO_ENCODED_RUSTFLAGS \
    CODEX_NODE PNPM_HOME TMPDIR HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY \
    http_proxy https_proxy all_proxy no_proxy; do
    if [[ -v $name ]]; then environment+=("--setenv=$name"); fi
  done
  echo "Delivery unit: $DELIVERY_UNIT"
  echo "Delivery logs: journalctl --user --unit=$DELIVERY_UNIT --no-pager"
  # Never use --pipe: the caller's death must not close the worker's output.
  # systemd-run --wait propagates the service's exit status when the caller lives.
  status=0
  systemd-run --user --unit="$DELIVERY_UNIT" --collect --wait --same-dir \
    --service-type=exec --expand-environment=no --property=StandardOutput=journal \
    --property=StandardError=journal "${environment[@]}" \
    -- "$BASH" "$script" "$commit" || status=$?
  journalctl --user --unit="$DELIVERY_UNIT" --no-pager --output=cat || {
    echo "could not replay delivery logs; use the journal command above" >&2
  }
  exit "$status"
fi

# The unit journal records an outcome even for a failed build, lock, or rollback.
trap 'status=$?; echo "delivery $commit finished with exit code $status"' EXIT

root=${CODEX_STUDIO_SERVER_ROOT:-$HOME/${layout[3]}}
release=$root/releases/$commit
binary=${layout[2]}
current=$root/${layout[4]}
dropin=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$UNIT.d/zz-delivery.conf

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
  ln -sfn "$1" "$current.next"
  mv -T "$current.next" "$current"
  mkdir -p "$(dirname "$dropin")"
  node -e 'const binary=process.argv[1]; if(/[\r\n\0]/.test(binary)) throw Error("invalid service executable path"); const quoted=binary.replaceAll("\\", "\\\\").replaceAll("\"", "\\\"").replaceAll("%", "%%").replaceAll("$", "$$$$"); process.stdout.write(`[Service]\nExecStart=\nExecStart="${quoted}" --port ${process.argv[2]}\n`)' \
    "$current/$binary" "$PORT" >"$dropin"
  systemctl --user daemon-reload
  systemctl --user restart "$UNIT"
}

previous=$(readlink "$current" || true)
start "$release"
if ! healthy "$release"; then
  echo "release $commit is unhealthy; rolling back" >&2
  if [ -n "$previous" ]; then
    start "$previous"
  else
    rm -f "$current" "$dropin"
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
