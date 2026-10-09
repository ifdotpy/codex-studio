#!/bin/sh
# Planned supervisor cutover/restart. This never signals the supervisor.
set -eu
if [ "${CODEX_AGENTS_SUPERVISOR_MODE:-0}" != "1" ]; then
  echo "Set CODEX_AGENTS_SUPERVISOR_MODE=1 after installing the supervisor service." >&2
  exit 2
fi
PYTHON=${CODEX_AGENTS_PYTHON:-python3}
exec "$PYTHON" -B - "$@" <<'PY'
import hashlib, json, os, plistlib, re, signal, subprocess, sys, time, urllib.request
from pathlib import Path

state = Path(os.environ.get("CODEX_AGENTS_STATE_DIR", Path.home() / ".local/state/codex-agents")).expanduser().resolve()
port = int(os.environ.get("CODEX_DESKTOP_PORT", "4620"))
origin = f"http://127.0.0.1:{port}/api/desktop"

def get():
    with urllib.request.urlopen(origin, timeout=3) as response:
        return json.load(response)

def verify_recovery():
    if sys.platform != "darwin":
        return "not_checked"
    try:
        config_path = state / "background-recovery.json"
        config = json.loads(config_path.read_text())
        if (config.get("version") != 1 or config.get("enabled") is not True
                or config.get("supervisorEnabled") is not True
                or config.get("stateDir") != str(state)
                or type(config.get("port")) is not int or config["port"] != port):
            raise ValueError("saved recovery settings do not match this backend")
        resources = Path(config["resources"])
        python = Path(config["python"])
        codex = Path(config["codex"])
        helper = resources.parent / "recover_backend.py"
        if (not resources.is_absolute() or not python.is_absolute() or not codex.is_absolute()
                or not os.access(python, os.X_OK) or not os.access(codex, os.X_OK)
                or not helper.is_file() or not (resources / "scripts/codex-canvas").is_file()
                or not (resources / "web/dist/index.html").is_file()):
            raise ValueError("saved recovery paths are unavailable")
        label = "local.codex.agents.recovery." + hashlib.sha256(str(state).encode()).hexdigest()[:16]
        plist_path = Path.home() / "Library/LaunchAgents" / (label + ".plist")
        plist = plistlib.loads(plist_path.read_bytes())
        expected = ["-B", str(helper), "--config", str(config_path)]

        def valid_arguments(arguments):
            # A registered job can retain its interpreter after a source update.
            return (isinstance(arguments, list) and len(arguments) == 5
                    and all(isinstance(item, str) for item in arguments)
                    and Path(arguments[0]).is_absolute()
                    and os.access(arguments[0], os.X_OK)
                    and arguments[1:] == expected)

        if (plist.get("Label") != label or plist.get("KeepAlive") is not True
                or plist.get("RunAtLoad") is not True
                or not valid_arguments(plist.get("ProgramArguments"))):
            raise ValueError("saved recovery service does not match this backend")
        domain = f"gui/{os.getuid()}"
        service = f"{domain}/{label}"

        def launchctl(*arguments):
            return subprocess.run(["/bin/launchctl", *arguments], check=True,
                                  capture_output=True, text=True, timeout=5)

        try:
            registered = launchctl("print", service)
        except subprocess.CalledProcessError as error:
            if error.returncode != 113:
                raise
            try:
                launchctl("bootstrap", domain, str(plist_path))
            except (OSError, subprocess.SubprocessError):
                # A lost response does not prove registration failed. Read it back.
                pass
            registered = launchctl("print", service)
        arguments = re.search(r"^\s*arguments = \{\s*\n(.*?)^\s*\}",
                              registered.stdout, re.MULTILINE | re.DOTALL)
        if arguments is None or not valid_arguments(
                [line.strip() for line in arguments.group(1).splitlines()]):
            raise ValueError("registered recovery service does not match this backend")
        return "verified"
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        raise SystemExit(f"Recovery service preflight failed: {error}; no process was signaled.")

if set(sys.argv[1:]) - {"--initial-cutover", "--check-only"}:
    raise SystemExit("Unknown restart option; no process was signaled.")
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
recovery = verify_recovery()
try:
    confirmed = get()
except Exception as error:
    raise SystemExit(f"Backend identity readback failed: {error}; no process was signaled.")
if (any(confirmed.get(key) != before.get(key) for key in
        ("application", "stateDir", "pid", "supervisorMode"))
        or (confirmed.get("supervisor") or {}).get("protocol") != 1
        or (confirmed.get("supervisor") or {}).get("stateDir") != str(state)):
    raise SystemExit("Backend identity changed during preflight; no process was signaled.")
if "--check-only" in sys.argv[1:]:
    print(json.dumps({"pid": before["pid"], "stateDir": str(state), "recovery": recovery}))
    raise SystemExit(0)
try:
    os.kill(before["pid"], signal.SIGTERM)
except ProcessLookupError:
    pass

end = time.monotonic() + 300
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
    raise SystemExit("The recovery service did not replace the backend within 300 seconds.")
PY
