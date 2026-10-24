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
# Recent completed invocation identity records retained, plus any still in use.
readonly KEPT_INVOCATION_RECORDS=20
# First systemd version supporting --expand-environment=no.
readonly MIN_SYSTEMD_VERSION=254
# Namespace for independently managed delivery services.
readonly DELIVERY_UNIT_PREFIX=codex-studio-deliver
# systemd InvocationID is a 128-bit identity encoded as 32 hexadecimal digits.
readonly SYSTEMD_INVOCATION_ID_PATTERN='^[[:xdigit:]]{32}$'
# Kernel UUIDs, with separators removed, identify individual delivery attempts.
readonly DELIVERY_UUID_PATTERN='^[[:xdigit:]]{32}$'

repository=$(git rev-parse --show-toplevel)
commit=$(git -C "$repository" rev-parse --verify "${1:-HEAD}^{commit}")
root=${CODEX_STUDIO_SERVER_ROOT:-$HOME/${layout[3]}}
script_directory=$(cd -- "$(dirname -- "$0")" && pwd)
invocation_directory=$root/delivery-invocations
invocation_history_lock=$root/delivery-invocations.lock
invocation_record_open=false

# The caller and worker each keep a shared lock until they finish using the
# record. Serialize record creation with pruning to close the open/lock race.
open_invocation_record() {
  mkdir -p "$invocation_directory"
  exec 6>"$invocation_history_lock"
  flock 6
  (umask 077; : >> "$CODEX_STUDIO_DELIVER_INVOCATION_FILE")
  exec 8<>"$CODEX_STUDIO_DELIVER_INVOCATION_FILE"
  flock -s 8
  invocation_record_open=true
  exec 6>&-
}

prune_invocation_records() (
  [ -d "$invocation_directory" ] || exit 0
  exec 6>"$invocation_history_lock"
  flock 6
  records=("$invocation_directory"/*)
  [ -f "${records[0]}" ] || exit 0
  ls -1t -- "${records[@]}" | tail -n +$((KEPT_INVOCATION_RECORDS + 1)) |
    while IFS= read -r old; do
      # An old record may still belong to a running worker or a caller that
      # has not replayed its output. Never wait on or remove such a record.
      (
        exec 7<"$old"
        flock -xn 7 || exit 0
        rm -f -- "$old"
      )
    done
)

# Trust the recursion guard only inside the named transient service. Ambient
# environment from a server or a previous delivery does not prove membership.
in_delivery_unit=false
if [ "${CODEX_STUDIO_DELIVER_IN_UNIT:-}" = 1 ] &&
   [[ ${INVOCATION_ID:-} =~ $SYSTEMD_INVOCATION_ID_PATTERN ]] &&
   [[ ${CODEX_STUDIO_DELIVER_UNIT:-} == "$DELIVERY_UNIT_PREFIX"-*.service ]]; then
  while IFS=: read -r _ _ cgroup; do
    if [[ "$cgroup/" == *"/$CODEX_STUDIO_DELIVER_UNIT/"* ]]; then
      in_delivery_unit=true
      break
    fi
  done < /proc/self/cgroup
fi
# Tests copy this script and layout beside an untracked fixture-only marker.
# An inherited test flag has no effect in the production source checkout.
test_fixture=false
if [ "${CODEX_STUDIO_DELIVER_TEST_IN_PROCESS:-}" = 1 ] &&
   [ -f "$script_directory/.codex-studio-delivery-test" ]; then
  test_fixture=true
fi
if [ "${CODEX_STUDIO_DELIVER_IN_UNIT:-}" = 1 ] &&
   [ "$in_delivery_unit" != true ] && [ "$test_fixture" != true ]; then
  echo "delivery guard requires verified unit membership; refusing in-process delivery" >&2
  exit 1
fi

# A service restart kills descendants of the old server, including its command
# monitors. Hand off before taking the lock or building so the entire delivery
# belongs to an independent service. Only disposable fixtures may bypass this.
if [ "$in_delivery_unit" != true ] && [ "$test_fixture" != true ]; then
  command -v systemd-run >/dev/null || {
    echo "systemd-run is required to deliver outside the server's cgroup" >&2
    exit 1
  }
  command -v journalctl >/dev/null || {
    echo "journalctl is required to read delivery output" >&2
    exit 1
  }
  version_output=$(systemd-run --version) || {
    echo "could not determine systemd-run version" >&2
    exit 1
  }
  read -r _ systemd_version _ <<<"$version_output"
  if ! [[ $systemd_version =~ ^[0-9]+$ ]] ||
     [ "$systemd_version" -lt "$MIN_SYSTEMD_VERSION" ]; then
    echo "systemd $MIN_SYSTEMD_VERSION or newer is required for literal delivery arguments" >&2
    exit 1
  fi
  IFS= read -r unique_id < /proc/sys/kernel/random/uuid
  unique_id=${unique_id//-/}
  [[ $unique_id =~ $DELIVERY_UUID_PATTERN ]] || {
    echo "could not create a unique delivery identity" >&2
    exit 1
  }
  # One owner for the unique transient service identity and its journal lookup.
  readonly DELIVERY_UNIT=$DELIVERY_UNIT_PREFIX-${commit:0:12}-$unique_id.service
  export CODEX_STUDIO_DELIVER_UNIT=$DELIVERY_UNIT
  export CODEX_STUDIO_DELIVER_INVOCATION_FILE=$invocation_directory/$unique_id
  export CODEX_STUDIO_SERVER_ROOT=$root
  script=$script_directory/${0##*/}
  environment=(--setenv=CODEX_STUDIO_DELIVER_IN_UNIT=1)
  # A user manager does not inherit the invoking shell's toolchain and paths.
  # Pass variable names rather than exposing their values in the argument list.
  for name in HOME PATH XDG_CONFIG_HOME XDG_CACHE_HOME XDG_DATA_HOME \
    XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS CODEX_STUDIO_SERVER_ROOT \
    CODEX_STUDIO_DELIVER_UNIT CODEX_STUDIO_DELIVER_INVOCATION_FILE \
    CARGO_HOME RUSTUP_HOME CARGO_BUILD_JOBS RUSTFLAGS CARGO_ENCODED_RUSTFLAGS \
    CODEX_NODE PNPM_HOME TMPDIR HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY \
    http_proxy https_proxy all_proxy no_proxy; do
    if [[ -v $name ]]; then environment+=("--setenv=$name"); fi
  done
  open_invocation_record
  echo "Delivery unit: $DELIVERY_UNIT"
  echo "Delivery logs: journalctl --user --unit=$DELIVERY_UNIT --no-pager"
  # Never use --pipe: the caller's death must not close the worker's output.
  # systemd-run --wait propagates the service's exit status when the caller lives.
  status=0
  systemd-run --user --unit="$DELIVERY_UNIT" --collect --wait --same-dir \
    --service-type=exec --expand-environment=no --property=StandardOutput=journal \
    --property=StandardError=journal "${environment[@]}" \
    -- "$BASH" "$script" "$commit" || status=$?
  unit_invocation=
  if [ -f "$CODEX_STUDIO_DELIVER_INVOCATION_FILE" ] &&
     IFS= read -r unit_invocation < "$CODEX_STUDIO_DELIVER_INVOCATION_FILE" &&
     [[ $unit_invocation =~ $SYSTEMD_INVOCATION_ID_PATTERN ]]; then
    journalctl --user --unit="$DELIVERY_UNIT" \
      "_SYSTEMD_INVOCATION_ID=$unit_invocation" --no-pager --output=cat || {
      echo "could not replay delivery logs; use the journal command above" >&2
    }
  else
    echo "delivery unit did not record a verified invocation; inspect its journal" >&2
    [ "$status" -ne 0 ] || status=1
  fi
  # Completion/replay makes this the most recent record, even after a long run.
  touch -- "$CODEX_STUDIO_DELIVER_INVOCATION_FILE"
  exec 8>&-
  prune_invocation_records || {
    echo "could not prune delivery invocation history" >&2
    [ "$status" -ne 0 ] || status=1
  }
  exit "$status"
fi

# The worker also prunes on failure or after losing its caller. Its shared lock
# can be released now; a surviving caller still protects its pending replay.
finish_delivery() {
  local status=$?
  if [ "$invocation_record_open" = true ]; then
    touch -- "$CODEX_STUDIO_DELIVER_INVOCATION_FILE" || {
      echo "could not update delivery invocation history" >&2
      [ "$status" -ne 0 ] || status=1
    }
    exec 8>&-
    prune_invocation_records || {
      echo "could not prune delivery invocation history" >&2
      [ "$status" -ne 0 ] || status=1
    }
  fi
  echo "delivery $commit finished with exit code $status"
  exit "$status"
}
trap finish_delivery EXIT

# Record the manager's invocation identity before any build or service changes.
# The caller can read it after --collect has removed the completed unit.
if [ -n "${CODEX_STUDIO_DELIVER_INVOCATION_FILE:-}" ]; then
  [[ ${INVOCATION_ID:-} =~ $SYSTEMD_INVOCATION_ID_PATTERN ]] || {
    echo "delivery unit has no valid manager invocation identity" >&2
    exit 1
  }
  open_invocation_record
  printf '%s\n' "$INVOCATION_ID" > "$CODEX_STUDIO_DELIVER_INVOCATION_FILE"
  echo "Delivery invocation: $INVOCATION_ID"
  prune_invocation_records
fi

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
