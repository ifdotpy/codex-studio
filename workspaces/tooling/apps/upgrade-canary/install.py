"""Run the real update installer of build N against the canary's application folder.

The canary stages build N+1 in its clone state the way a paired server does
(pending-update.json and a staged copy), then calls this file inside its installer
sandbox. It loads install_update.py of the installed build N and calls its install()
with two changes only: `applications` is the canary folder (never /Applications), and
the tool runner answers "no window" for osascript and does not run open, because the
bundle id would match the owner's live window. Every other step is the real code:
verify, ditto, swap, backend stop.

usage: python3 -B install.py --resources DIR --config FILE --applications DIR
Prints {"code": ..., "detail": ...}; exit 0 only when the build was installed.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

parser = argparse.ArgumentParser()
parser.add_argument("--resources", type=Path, required=True)
parser.add_argument("--config", type=Path, required=True)
parser.add_argument("--applications", type=Path, required=True)
args = parser.parse_args()

config_path = args.config.resolve()
state = config_path.parent
applications = args.applications.resolve()
if applications == Path("/Applications") or applications.parent != state.parent:
    sys.exit(f"refusing: {applications} is not the canary application folder")
resources = args.resources.resolve()
if not resources.is_relative_to(applications):
    sys.exit(f"refusing: {resources} is not inside {applications}")

# Builds before the helper move import their update checks from the backend scripts.
for folder in (resources / "workspace" / "scripts", resources):
    sys.path.insert(0, str(folder))
spec = importlib.util.spec_from_file_location("canary_install_update", resources / "install_update.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

config = installer.recovery.load_config(config_path, state)
updates, _vm = installer.load_updates(config["resources"])


def run(argv, timeout):
    """The real tool runner, except the window: the canary has none."""
    if argv and argv[0] == installer.OSASCRIPT:
        return subprocess.CompletedProcess(argv, 0, "false\n", "")
    if argv and argv[0] == installer.OPEN:
        return subprocess.CompletedProcess(argv, 0, "", "")
    return updates.run_tool(argv, timeout)


def verify(config, state):
    """The real backend proof; a failure also prints its cause, which install() folds away."""
    try:
        return installer.verify_backend(config, state)
    except Exception as error:
        print(json.dumps({"event": "canary-verify-failed", "error": str(error),
                          "cause": repr(error.__cause__)}), flush=True)
        raise


code, detail = installer.install(state, config, applications=applications, run=run, verify=verify,
                                 target=lambda *_: None)
print(json.dumps({"code": code, "detail": detail}), flush=True)
sys.exit(0 if code == installer.INSTALLED else 1)
