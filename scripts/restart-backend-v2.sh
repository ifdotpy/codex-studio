#!/bin/sh
# Planned supervisor cutover/restart. This never signals the supervisor.
set -eu
if [ "${CODEX_AGENTS_SUPERVISOR_MODE:-0}" != "1" ]; then
  echo "Set CODEX_AGENTS_SUPERVISOR_MODE=1 after installing the supervisor service." >&2
  exit 2
fi
PYTHON=${CODEX_AGENTS_PYTHON:-python3}
exec "$PYTHON" -B - "$@" <<'PY'
import json, os, signal, sys, time, urllib.request
from pathlib import Path

state = Path(os.environ.get("CODEX_AGENTS_STATE_DIR", Path.home() / ".local/state/codex-agents")).expanduser().resolve()
port = int(os.environ.get("CODEX_DESKTOP_PORT", "4620"))
origin = f"http://127.0.0.1:{port}/api/desktop"

def get():
    with urllib.request.urlopen(origin, timeout=3) as response:
        return json.load(response)

try:
    before = get()
except Exception as error:
    raise SystemExit(f"Backend identity preflight failed: {error}")
if before.get("application") != "codex-agents" or before.get("stateDir") != str(state):
    raise SystemExit("Backend identity or state directory mismatch; no process was signaled.")
supervisor = before.get("supervisor") or {}
if supervisor.get("protocol") != 1 or supervisor.get("stateDir") != str(state):
    raise SystemExit("Supervisor is missing or incompatible; no process was signaled.")
initial_cutover = "--initial-cutover" in sys.argv[1:]
if before.get("supervisorMode") is not True:
    if not initial_cutover:
        raise SystemExit("Backend is not in supervisor mode; use --initial-cutover only at the planned idle boundary.")
    if supervisor.get("handles"):
        raise SystemExit("Supervisor already owns native handles; initial cutover requires an empty handle journal.")
elif initial_cutover:
    raise SystemExit("Initial cutover was requested, but this backend already uses supervisor mode.")
try:
    os.kill(before["pid"], signal.SIGTERM)
except ProcessLookupError:
    pass

end = time.monotonic() + 60
while time.monotonic() < end:
    try:
        current = get()
    except Exception:
        time.sleep(.25)
        continue
    if current.get("stateDir") != str(state) or current.get("application") != "codex-agents":
        raise SystemExit("Replacement backend identity changed; inspect recovery logs.")
    if current.get("pid") != before.get("pid"):
        if current.get("supervisorMode") is not True or current.get("supervisor", {}).get("protocol") != 1:
            raise SystemExit("Replacement backend did not reattach to the supervisor.")
        print(json.dumps({"previousPid": before["pid"], "pid": current["pid"],
                          "stateDir": str(state), "supervisor": "attached"}))
        break
    time.sleep(.25)
else:
    raise SystemExit("The recovery service did not replace the backend within 60 seconds.")
PY
