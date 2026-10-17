"""Start the packaged host hidden, with isolated client and backend state."""
import json
import os
from pathlib import Path
import plistlib
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

bundle = Path(sys.argv[1]).resolve()
info = plistlib.loads((bundle / "Contents/Info.plist").read_bytes())
executable = bundle / "Contents/MacOS" / info["CFBundleExecutable"]
fixture = Path(tempfile.mkdtemp(prefix="studio-native-pkg-", dir="/tmp")).resolve()
state = fixture / "state"
with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
env = dict(os.environ, CODEX_AGENTS_STATE_DIR=str(state),
           CODEX_DESKTOP_PROFILE=str(fixture / "profile"),
           CODEX_DESKTOP_EPHEMERAL_PROFILE="1", CODEX_DESKTOP_PORT=str(port),
           CODEX_AGENTS_SUPERVISOR_MODE="0")
identity = None
owned_pid = None
host = None
try:
    with (fixture / "host.log").open("w") as log:
        # AppKit restores state by bundle ID, outside CODEX_DESKTOP_PROFILE.
        # Its documented test-only argument isolates that state too, so a prior
        # test termination cannot open a modal restore prompt before setup.
        # https://developer.apple.com/library/archive/releasenotes/AppKit/RN-AppKitOlderNotes/
        host = subprocess.Popen([str(executable), "--hidden", "-ApplePersistenceIgnoreState", "YES"], env=env,
                                stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if host.poll() is not None:
                raise RuntimeError(f"Packaged host exited with {host.returncode}")
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/desktop", timeout=1) as response:
                    candidate = json.load(response)
                if candidate.get("application") != "codex-agents" or candidate.get("stateDir") != str(state):
                    raise RuntimeError("The listener belongs to another runtime")
                identity = candidate
                break
            except (urllib.error.URLError, TimeoutError):
                time.sleep(0.1)
        if identity is None:
            raise RuntimeError("The packaged UI did not start its backend within 60 seconds")
        if type(identity.get("pid")) is not int or identity["pid"] <= 1:
            raise RuntimeError("Invalid backend process identity")
        command = subprocess.check_output(["/bin/ps", "-p", str(identity["pid"]), "-o", "command="], text=True)
        expected = bundle / "Contents/Resources/workspace/scripts/codex-canvas"
        if str(expected) not in command:
            raise RuntimeError("Backend did not start from this bundle")
        owned_pid = identity["pid"]
        assert (fixture / "profile/client/local").is_dir()
        print(json.dumps({"result": "PASS", "bundle": str(bundle), "evidence": str(fixture),
                          "backendPid": identity["pid"], "stateDir": identity["stateDir"],
                          "checks": "hidden packaged host started its bundled backend in isolated state"}))
finally:
    if host is not None and host.poll() is None:
        host.terminate()
        try:
            host.wait(timeout=10)
        except subprocess.TimeoutExpired:
            host.kill()
            host.wait()
    # Only a runtime whose identity proved ownership of this fresh fixture.
    if owned_pid is not None:
        try:
            os.kill(owned_pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    print(f"Package startup evidence: {fixture}", file=sys.stderr)
