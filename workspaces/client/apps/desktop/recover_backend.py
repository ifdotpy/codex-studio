#!/usr/bin/env python3
"""Keep the saved Studio backend available without taking over an occupied runtime."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def backend_scripts(resources):
    """Resolve either packaged resource names or the relocated source tree."""
    root = Path(resources)
    packaged = root / "scripts"
    development = root / "workspaces" / "runtime" / "apps" / "server" / "src"
    if (development / "codex-canvas").is_file():
        return development
    if (packaged / "codex-canvas").is_file() or (root / "studio-install.json").is_file():
        return packaged
    return root / "workspaces" / "runtime" / "apps" / "server" / "src"


def renderer_dist(resources):
    root = Path(resources)
    development = root / "workspaces" / "client" / "apps" / "web" / "dist"
    return development if (development / "index.html").is_file() else root / "web" / "dist"


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


def backend_process_arguments(pid, resources):
    """Reuse the packaged native process reader without parsing display text."""
    scripts = backend_scripts(resources).resolve()
    sys.path.insert(0, str(scripts))
    try:
        import codex_process_supervisor
        if Path(codex_process_supervisor.__file__).resolve() != scripts / "codex_process_supervisor.py":
            raise RuntimeError("The packaged process reader is unavailable.")
        return codex_process_supervisor.process_launch_command(pid)
    finally:
        sys.path.remove(str(scripts))


def verify_backend_process(data, config):
    """Prove the reported PID is the packaged backend listening on this port."""
    pid = data["pid"]
    script = (backend_scripts(config["resources"]) / "codex-canvas").resolve()
    try:
        arguments = backend_process_arguments(pid, config["resources"])
    except (OSError, ValueError, RuntimeError, ImportError) as error:
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    if str(script) not in arguments:
        raise RuntimeError("The reported backend PID is not the packaged Codex Canvas process.")
    try:
        listeners = subprocess.check_output(
            ["/usr/sbin/lsof", "-nP", f"-iTCP:{config['port']}", "-sTCP:LISTEN", "-t"],
            text=True, stderr=subprocess.DEVNULL, timeout=2,
        ).split()
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError("Cannot verify the backend listener before fallback.") from error
    if str(pid) not in listeners:
        raise RuntimeError("The reported backend PID does not own the configured listener.")


def lease_available(state):
    # This probe is not ownership. Runtime acquires its own authoritative lease.
    with (state / "runtime.lock").open("a+") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        fcntl.flock(lease, fcntl.LOCK_UN)
        return True


def process_start_time(pid):
    try:
        value = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "lstart="],
                                        text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
        state = subprocess.check_output(["/bin/ps", "-p", str(pid), "-o", "stat="],
                                        text=True, stderr=subprocess.DEVNULL, timeout=2).strip()
    except subprocess.CalledProcessError as error:
        if error.returncode == 1:
            return None
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    return value if value and not state.startswith("Z") else None


def load_config(filename, state):
    config = json.loads(filename.read_text())
    if config.get("version") != 1 or Path(config["stateDir"]).resolve() != state:
        raise RuntimeError("The recovery configuration uses a different state directory.")
    if "supervisorEnabled" in config and type(config["supervisorEnabled"]) is not bool:
        raise RuntimeError("The recovery supervisor setting is invalid.")
    if type(config.get("port")) is not int or not 1024 <= config["port"] <= 65535:
        raise RuntimeError("The recovery port is invalid.")
    for name in ("python", "codex", "resources"):
        if not Path(config[name]).is_absolute():
            raise RuntimeError(f"The recovery {name} path must be absolute.")
    return config


def launch_environment(config, state, supervisor_fallback=False):
    env = dict(os.environ)
    for key in config.get("unsetEnvironment", []):
        env.pop(key, None)
    env.update(config.get("environment", {}))
    env.update({"CODEX_AGENTS_STATE_DIR": str(state), "CODEX_BIN": config["codex"]})
    if supervisor_fallback:
        env.pop("CODEX_AGENTS_SUPERVISOR_MODE", None)
        env["CODEX_AGENTS_SUPERVISOR_FALLBACK"] = "1"
    else:
        env.pop("CODEX_AGENTS_SUPERVISOR_FALLBACK", None)
        if config.get("supervisorEnabled") is True:
            env["CODEX_AGENTS_SUPERVISOR_MODE"] = "1"
        else:
            env.pop("CODEX_AGENTS_SUPERVISOR_MODE", None)
    return env


def supervisor_tick(config, state, child=None):
    """Probe the separately supervised owner without taking over its lifecycle."""
    if config.get("supervisorEnabled") is not True:
        return child, "disabled"
    script = backend_scripts(config["resources"]) / "codex_process_supervisor.py"
    if not script.is_file():
        raise RuntimeError("Supervisor mode is enabled but its packaged script is missing.")
    check = [config["python"], "-B", str(script), "--status-json", "--state", str(state)]
    try:
        probe = subprocess.run(check, check=True, timeout=2, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, env=launch_environment(config, state))
        health = json.loads(probe.stdout)
        recovery = health.get("recovery") or {}
        if recovery.get("blocked"):
            raise RuntimeError(recovery["blocked"])
        if recovery.get("degraded") and recovery.get("fallbackReady"):
            return child, "supervisor degraded"
        return child, "supervisor ready"
    except (OSError, subprocess.SubprocessError):
        # A failed probe does not prove that the owner is dead. The dedicated
        # supervisor LaunchAgent owns restart and keeps recovery probes passive.
        return child, "supervisor starting"


def tick(config, state, child=None, supervisor_fallback=False):
    if child is not None and child.poll() is None:
        return child, "starting"
    existing = identity(config["port"], state)
    if existing and supervisor_fallback and existing.get("supervisorFallback") is True:
        return None, "fallback attached"
    if existing and supervisor_fallback and existing.get("supervisorMode") is True:
        verify_backend_process(existing, config)
        os.kill(existing["pid"], signal.SIGTERM)
        return None, "supervisor backend stopping for fallback"
    if existing:
        return None, "attached"
    if not lease_available(state):
        return None, "waiting for runtime owner"
    resources = Path(config["resources"])
    script = backend_scripts(resources) / "codex-canvas"
    if not script.is_file() or not (renderer_dist(resources) / "index.html").is_file():
        raise RuntimeError("The installed backend or web assets are unavailable.")
    env = launch_environment(config, state, supervisor_fallback=supervisor_fallback)
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
        supervisor = None
        desktop = None
        fallback_pid = None
        fallback_start = None
        desktop_attempt = 0
        previous = None
        while not stopping:
            try:
                config = load_config(filename, state)
                if config.get("enabled") is not True:
                    return
                if "CODEX_AGENTS_SUPERVISOR_MODE" in os.environ:
                    config["supervisorEnabled"] = (
                        os.environ["CODEX_AGENTS_SUPERVISOR_MODE"] == "1"
                    )
                supervisor, supervisor_status = supervisor_tick(config, state, supervisor)
                if supervisor_status == "supervisor degraded":
                    existing = identity(config["port"], state)
                    fallback_exited = (fallback_pid is not None and fallback_start is not None
                                       and process_start_time(fallback_pid) != fallback_start)
                    if fallback_exited:
                        script = backend_scripts(config["resources"]) / "codex_process_supervisor.py"
                        subprocess.run([config["python"], "-B", str(script), "--finish-fallback",
                                        "--state", str(state)], check=True, timeout=3,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                        fallback_pid = fallback_start = None
                        supervisor_status = "supervisor ready"
                        child, status = tick(config, state, None)
                    elif existing and existing.get("supervisorFallback") is True:
                        fallback_pid = existing["pid"]
                        fallback_start = process_start_time(fallback_pid)
                        status = "fallback backend attached"
                    elif existing and existing.get("supervisorMode") is True:
                        os.kill(existing["pid"], signal.SIGTERM)
                        status = "supervisor backend stopping for fallback"
                    elif child is not None and child.poll() is None:
                        status = "fallback backend starting"
                    else:
                        child, status = tick(config, state, child, supervisor_fallback=True)
                        if status == "started":
                            fallback_pid = child.pid
                            fallback_start = process_start_time(fallback_pid)
                elif config.get("supervisorEnabled") is True and supervisor_status != "supervisor ready":
                    status = supervisor_status
                else:
                    child, status = tick(config, state, child)
                    if supervisor_status == "supervisor ready":
                        status = f"{status}; {supervisor_status}"
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
