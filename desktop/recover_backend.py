#!/usr/bin/env python3
"""Keep the saved Studio backend available without taking over an occupied runtime."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def identity(port, state, timeout=5):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(f"http://127.0.0.1:{port}/api/desktop", timeout=timeout) as response:
            data = json.load(response)
    except urllib.error.URLError as error:
        if isinstance(error.reason, ConnectionRefusedError):
            return None
        raise
    if (data.get("application") != "codex-agents" or data.get("protocol") != 1
            or data.get("stateDir") != str(state) or type(data.get("pid")) is not int
            or data["pid"] < 1):
        raise RuntimeError("The backend identity does not match this recovery service.")
    os.kill(data["pid"], 0)
    return data


def lease_available(state):
    # This probe is not ownership. Runtime acquires its own authoritative lease.
    with (state / "runtime.lock").open("a+") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        fcntl.flock(lease, fcntl.LOCK_UN)
        return True


def load_config(filename, state):
    config = json.loads(filename.read_text())
    if config.get("version") != 1 or Path(config["stateDir"]).resolve() != state:
        raise RuntimeError("The recovery configuration uses a different state directory.")
    if type(config.get("port")) is not int or not 1024 <= config["port"] <= 65535:
        raise RuntimeError("The recovery port is invalid.")
    for name in ("python", "codex", "resources"):
        if not Path(config[name]).is_absolute():
            raise RuntimeError(f"The recovery {name} path must be absolute.")
    return config


def launch_environment(config, state):
    env = dict(os.environ)
    for key in config.get("unsetEnvironment", []):
        env.pop(key, None)
    env.update(config.get("environment", {}))
    env.update({"CODEX_AGENTS_STATE_DIR": str(state), "CODEX_BIN": config["codex"]})
    return env


def tick(config, state, child=None):
    if child is not None and child.poll() is None:
        return child, "starting"
    existing = identity(config["port"], state)
    if existing:
        return None, "attached"
    if not lease_available(state):
        return None, "waiting for runtime owner"
    resources = Path(config["resources"])
    script = resources / "scripts/codex-canvas"
    if not script.is_file() or not (resources / "web/dist/index.html").is_file():
        raise RuntimeError("The installed backend or web assets are unavailable.")
    env = launch_environment(config, state)
    with (state / "canvas.log").open("ab", buffering=0) as log:
        child = subprocess.Popen([config["python"], "-B", str(script), "--port", str(config["port"])],
                                 cwd=Path.home(), env=env, stdin=subprocess.DEVNULL,
                                 stdout=log, stderr=log, start_new_session=True)
    return child, "started"


def desktop_tick(config, state, child=None):
    """Restore only a saved open-window intent whose exact previous process ended."""
    if child is not None and child.poll() is None:
        return child, "desktop running"
    filename = state / "desktop-recovery.json"
    if not filename.exists():
        return None, "desktop closed"
    intent = json.loads(filename.read_text())
    if intent.get("version") != 1 or intent.get("desiredOpen") is not True:
        return None, "desktop closed"
    pid = intent.get("pid")
    if type(pid) is not int or pid < 1 or not intent.get("startedAt"):
        raise RuntimeError("The saved desktop identity is invalid.")
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        pass
    else:
        started = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "lstart="], text=True, env={**os.environ, "LC_ALL": "C"}).strip()
        if not started:
            raise RuntimeError("The saved desktop process identity is unavailable.")
        if started == intent["startedAt"]:
            return None, "desktop attached"
    executable = Path(intent["executable"])
    profile = Path(intent["profile"])
    if not executable.is_absolute() or not executable.is_file() or not profile.is_absolute():
        raise RuntimeError("The saved desktop installation or profile is unavailable.")
    # Re-read the intent after probing. An explicit close can withdraw this request.
    if json.loads(filename.read_text()) != intent:
        return None, "desktop intent changed"
    env = launch_environment(config, state)
    env.update({"CODEX_DESKTOP_PORT": str(config["port"]), "CODEX_DESKTOP_PROFILE": str(profile)})
    with (state / "desktop-recovery.log").open("ab", buffering=0) as log:
        child = subprocess.Popen([str(executable), "--background-recovery"],
                                 cwd=Path.home(), env=env, stdin=subprocess.DEVNULL,
                                 stdout=log, stderr=log, start_new_session=True)
    return child, "desktop started"


def run(filename, interval=5):
    os.umask(0o077)
    state = filename.parent.resolve()
    stopping = False

    def stop(*_):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    with (state / "background-recovery.lock").open("a+") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return
        child = None
        desktop = None
        desktop_attempt = 0
        previous = None
        while not stopping:
            try:
                config = load_config(filename, state)
                if config.get("enabled") is not True:
                    return
                child, status = tick(config, state, child)
                if time.monotonic() >= desktop_attempt:
                    desktop, desktop_status = desktop_tick(config, state, desktop)
                    if desktop_status == "desktop started":
                        desktop_attempt = time.monotonic() + 60
                    status = f"{status}; {desktop_status}"
            except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
                # An unavailable or uncertain owner never authorizes a replacement.
                status = f"waiting: {error}"
            if status != previous:
                print(json.dumps({"event": "backend-recovery", "status": status}), flush=True)
                previous = status
            deadline = time.monotonic() + interval
            while not stopping and time.monotonic() < deadline:
                time.sleep(min(.2, max(0, deadline - time.monotonic())))
    # Disabling this service does not terminate the backend or its active work.


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    args = parser.parse_args()
    run(args.config.resolve())
