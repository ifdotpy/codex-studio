#!/usr/bin/env python3
"""Keep the saved Studio backend available without taking over an occupied runtime."""
import argparse
import fcntl
from dataclasses import dataclass
from enum import StrEnum
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request


# Supervisor protocol version that the Rust backend uses to start providers. Owner:
# workspaces/runtime/packages/runtime-providers/src/supervisor_protocol.rs (SUPERVISOR_PROTOCOL_V2);
# recovery-test.py checks this copy against it.
SUPERVISOR_PROTOCOL_V2 = 2
# Owner: MAX_SUPERVISOR_FRAME_BYTES in runtime-process/src/supervisor_contract.rs.
# recovery-test.py checks parity; includes the JSON line delimiter.
MAX_SUPERVISOR_FRAME_BYTES = 33554432


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def validate_identity(data, state):
    """Validate one desktop response before a recovery or delivery action."""
    if (data.get("application") != "codex-agents" or data.get("protocol") != 1
            or data.get("backendKind") != "rust" or data.get("stateDir") != str(state) or type(data.get("pid")) is not int
            or data["pid"] < 1):
        raise RuntimeError("The backend identity does not match this recovery service.")
    return data


def identity(port, state, timeout=5):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(f"http://127.0.0.1:{port}/api/desktop", timeout=timeout) as response:
            data = json.load(response)
    except urllib.error.URLError as error:
        if isinstance(error.reason, ConnectionRefusedError):
            return None
        raise
    validate_identity(data, state)
    os.kill(data["pid"], 0)
    return data


def backend_process_arguments(pid, resources):
    """Read exact native arguments through the desktop-owned process reader."""
    from desktop_process import process_launch_command
    return process_launch_command(pid)


def rust_server(config):
    """The configured Rust server, only when it is an executable file."""
    server = Path(config["runtimeServer"])
    if not server.is_file() or not os.access(server, os.X_OK):
        raise RuntimeError(f"The configured Rust server is missing or not executable: {server}")
    return server


def file_sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_backend_process(data, config):
    """Prove the reported PID is the configured backend listening on this port."""
    pid = data["pid"]
    try:
        arguments = backend_process_arguments(pid, config["resources"])
    except (OSError, ValueError, RuntimeError, ImportError) as error:
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    server = rust_server(config)
    if (data.get("backendKind") != "rust" or not arguments
            or Path(arguments[0]).resolve() != server.resolve()
            or data.get("backendBuild") != file_sha256(server)):
        raise RuntimeError("The reported backend PID is not the configured Rust server process.")
    verify_listener(pid, config)


def verify_listener(pid, config):
    """Prove that the PID owns the configured listener."""
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
    if config.get("backend", "rust") != "rust":
        raise RuntimeError("The recovery backend must be rust.")
    names = ["python", "codex", "resources", "runtimeServer"]
    if config.get("supervisorEnabled") is True:
        names.append("processSupervisor")
    for name in names:
        if type(config.get(name)) is not str or not Path(config[name]).is_absolute():
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
    binary = Path(config.get("processSupervisor") or "")
    if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError("Supervisor mode is enabled but the Rust process supervisor is missing.")
    try:
        health = supervisor_request(state, SupervisorAction.HEALTH)
    except OSError:
        # Failed probes never authorize replacing the separately supervised owner.
        return child, "supervisor starting"
    recovery = health.get("recovery") or {}
    if recovery.get("blocked"):
        raise RuntimeError(recovery["blocked"])
    if recovery.get("degraded") and recovery.get("fallbackReady"):
        return child, "supervisor degraded"
    return child, "supervisor ready"


# Seconds allowed for an administrative supervisor exchange.
SUPERVISOR_REQUEST_SECONDS = 2


@dataclass(frozen=True)
class SupervisorHello:
    ok: bool = False
    protocol: int | None = None
    error: str | None = None


@dataclass(frozen=True)
class SupervisorReply:
    requestId: int | None = None
    result: dict[str, object] | None = None
    error: str | None = None


class SupervisorAction(StrEnum):
    HEALTH = "health"
    FINISH_FALLBACK = "finishFallback"


def supervisor_request(state: Path, action: SupervisorAction) -> dict[str, object]:
    """Use the Rust supervisor's administrative socket without taking ownership."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(SUPERVISOR_REQUEST_SECONDS)
        client.connect(str(state / "supervisor.sock"))
        with client.makefile("rwb") as stream:
            def exchange(message: dict[str, object]) -> dict[str, object]:
                stream.write((json.dumps(message) + "\n").encode())
                stream.flush()
                line = stream.readline(MAX_SUPERVISOR_FRAME_BYTES + 1)
                if not line.endswith(b"\n") or len(line) > MAX_SUPERVISOR_FRAME_BYTES:
                    raise RuntimeError("The supervisor reply exceeds its limit or is incomplete.")
                return json.loads(line)

            hello = SupervisorHello(**exchange({"protocol": SUPERVISOR_PROTOCOL_V2,
                              "stateDir": str(state), "probe": True}))
            if hello.ok is not True or hello.protocol != SUPERVISOR_PROTOCOL_V2:
                raise RuntimeError("The running process supervisor does not serve protocol 2.")
            reply = SupervisorReply(**exchange({"requestId": 1, "action": action}))
            if reply.requestId != 1 or reply.error is not None or reply.result is None:
                raise RuntimeError(reply.error or "The supervisor reply does not match the request.")
            return reply.result


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
    command = [str(rust_server(config)), "--port", str(config["port"])]
    env = launch_environment(config, state, supervisor_fallback=supervisor_fallback)
    # A fresh Rust state accepts only a private canvas.log (accepted_private_file in
    # runtime-store fresh_state.rs); launchd starts this agent with umask 022.
    log_fd = os.open(state / "canvas.log", os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
    with os.fdopen(log_fd, "ab", buffering=0) as log:
        child = subprocess.Popen(command,
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


# Seconds between two installer runs while a staged update waits (docs/peer-updates.md).
UPDATE_RETRY_SECONDS = 60
# Seconds that one installer run may take.
UPDATE_INSTALL_SECONDS = 1800


def update_tick(filename, config, state, run=subprocess.run):
    """Run the installer of this app when the backend staged an update from a paired server."""
    from desktop_updates import PENDING_FILE
    if not (state / PENDING_FILE).is_file():
        return None
    installer = Path(__file__).resolve().with_name("install_update.py")
    if not installer.is_file():
        return "update installer missing"
    result = run([config["python"], "-B", str(installer), "--config", str(filename)],
                 cwd=Path.home(), env=launch_environment(config, state), stdin=subprocess.DEVNULL,
                 timeout=UPDATE_INSTALL_SECONDS, check=False)
    return {0: "update installed", 75: "update waiting"}.get(result.returncode, "update failed")


def own_code():
    """The bytes of this agent's files in the installed app, or None when they are unreadable."""
    try:
        here = Path(__file__).resolve()
        return tuple(path.read_bytes() for path in (here, here.with_name("install_update.py")) if path.exists())
    except OSError:
        return None


def run(filename, interval=5):
    os.umask(0o077)
    state = filename.parent.resolve()
    stopping = False
    # A manual install replaces the app under this running agent. Its code stays old in memory
    # until launchd starts it again, so it exits when its files changed.
    started_code = own_code()

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
        update_attempt = 0
        update_status = None
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
                        supervisor_request(state, SupervisorAction.FINISH_FALLBACK)
                        fallback_pid = fallback_start = None
                        supervisor_status = "supervisor ready"
                        child, status = tick(config, state, None)
                    elif existing and existing.get("supervisorFallback") is True:
                        fallback_pid = existing["pid"]
                        fallback_start = process_start_time(fallback_pid)
                        status = "fallback backend attached"
                    elif existing and existing.get("supervisorMode") is True:
                        verify_backend_process(existing, config)
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
                if time.monotonic() >= update_attempt:
                    update_attempt = time.monotonic() + UPDATE_RETRY_SECONDS
                    update_status = None
                    update_status = update_tick(filename, config, state)
                    if update_status == "update installed":
                        # launchd starts this agent again from the new app; it starts the backend.
                        print(json.dumps({"event": "backend-recovery", "status": update_status}), flush=True)
                        return
                if update_status:
                    status = f"{status}; {update_status}"
            except (OSError, ValueError, TypeError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
                # An unavailable or uncertain owner never authorizes a replacement.
                status = f"waiting: {error}"
            if status != previous:
                print(json.dumps({"event": "backend-recovery", "status": status}), flush=True)
                previous = status
            current_code = own_code()
            if started_code is not None and current_code is not None and current_code != started_code:
                # launchd starts this agent again from the installed app; the backend keeps running.
                print(json.dumps({"event": "backend-recovery", "status": "agent code changed"}), flush=True)
                return
            deadline = time.monotonic() + interval
            while not stopping and time.monotonic() < deadline:
                time.sleep(min(.2, max(0, deadline - time.monotonic())))
    # Disabling this service does not terminate the backend or its active work.


def check_interpreter():
    """Prove that this interpreter runs the recovery job: the imports above (StrEnum needs
    Python 3.11) and the helpers shipped beside this file."""
    import desktop_process  # noqa: F401
    import desktop_updates  # noqa: F401
    import desktop_vm  # noqa: F401
    print(json.dumps({"python": sys.executable, "version": list(sys.version_info[:3])}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path)
    parser.add_argument("--supervisor-health", action="store_true")
    parser.add_argument("--state", type=Path)
    parser.add_argument("--check-interpreter", action="store_true")
    args = parser.parse_args()
    if args.check_interpreter:
        check_interpreter()
    elif args.supervisor_health:
        if args.state is None:
            parser.error("--supervisor-health requires --state")
        print(json.dumps(supervisor_request(args.state.resolve(), SupervisorAction.HEALTH)))
    elif args.config is None:
        parser.error("--config is required")
    else:
        run(args.config.resolve())
