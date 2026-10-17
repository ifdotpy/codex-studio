#!/usr/bin/env python3
"""Keep the saved Studio backend available without taking over an occupied runtime."""
import argparse
import fcntl
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


# The `backend` field of background-recovery.json selects the server this agent starts.
# This module owns the field and validates it in `load_config`; the service writer in
# codex_supervisor_service.py (RECOVERY_BACKEND_SELECTIONS) is checked against it by
# test_codex_supervisor_service.py. A missing field means python.
BACKEND_SELECTIONS = ("python", "rust")
# Supervisor protocol version that the Rust backend uses to start providers. Owner:
# workspaces/runtime/packages/runtime-process/src/protocol.rs (SUPERVISOR_PROTOCOL_V2);
# recovery-test.py checks this copy against it.
SUPERVISOR_PROTOCOL_V2 = 2
# Bytes read for one supervisor handshake reply; the reply is one short JSON line.
SUPERVISOR_HANDSHAKE_REPLY_BYTES = 64 * 1024


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


def backend_selection(config):
    return config.get("backend", "python")


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
    rust = backend_selection(config) == "rust"
    try:
        arguments = backend_process_arguments(pid, config["resources"])
    except (OSError, ValueError, RuntimeError, ImportError) as error:
        raise RuntimeError(f"Cannot verify fallback backend PID {pid}") from error
    if rust:
        server = rust_server(config)
        # The Rust server reports the SHA-256 of its own executable as backendBuild.
        if (data.get("backendKind") != "rust" or not arguments
                or Path(arguments[0]).resolve() != server.resolve()
                or data.get("backendBuild") != file_sha256(server)):
            raise RuntimeError("The reported backend PID is not the configured Rust server process.")
    else:
        script = (backend_scripts(config["resources"]) / "codex-canvas").resolve()
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
    if config.get("backend", "python") not in BACKEND_SELECTIONS:
        raise RuntimeError("The recovery backend selection must be python or rust.")
    names = ["python", "codex", "resources"]
    if config.get("backend") == "rust":
        names.append("runtimeServer")
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


def supervisor_serves_protocol_v2(state):
    """Ask the supervisor socket for protocol 2 as a probe; a probe takes no ownership."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(2)
        client.connect(str(state / "supervisor.sock"))
        hello = {"protocol": SUPERVISOR_PROTOCOL_V2, "stateDir": str(state), "probe": True}
        client.sendall((json.dumps(hello) + "\n").encode())
        reply = b""
        while not reply.endswith(b"\n") and len(reply) < SUPERVISOR_HANDSHAKE_REPLY_BYTES:
            chunk = client.recv(4096)
            if not chunk:
                break
            reply += chunk
    if not reply.endswith(b"\n"):
        raise OSError("The supervisor closed the protocol probe without a reply.")
    answer = json.loads(reply)
    return (isinstance(answer, dict) and answer.get("ok") is True
            and answer.get("protocol") == SUPERVISOR_PROTOCOL_V2)


def supervisor_tick(config, state, child=None):
    """Probe the separately supervised owner without taking over its lifecycle."""
    if config.get("supervisorEnabled") is not True:
        return child, "disabled"
    script = backend_scripts(config["resources"]) / "codex_process_supervisor.py"
    if not script.is_file():
        raise RuntimeError("Supervisor mode is enabled but its packaged script is missing.")
    if backend_selection(config) == "rust":
        binary = Path(config.get("processSupervisor") or "")
        if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError("Supervisor mode is enabled but the Rust process supervisor is missing.")
        try:
            v2 = supervisor_serves_protocol_v2(state)
        except (OSError, ValueError):
            # An absent or starting socket does not prove the owner is wrong or dead.
            return child, "supervisor starting"
        if not v2:
            # A Python supervisor answers protocol 1 only. The Rust backend would start
            # without a provider path, so it waits for the Rust supervisor job instead.
            raise RuntimeError("The running process supervisor does not serve protocol 2; "
                               "the Rust backend needs the Rust supervisor.")
    # Both supervisors serve this protocol 1 health probe (the Rust one through supervisor_daemon.rs).
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
    selected = backend_selection(config)
    if existing:
        kind = existing.get("backendKind") or "python"
        if kind != selected:
            # Another backend owns this state. Recovery never replaces it; the selection
            # takes effect after an operator stops it (docs/restart-recovery.md).
            return None, f"attached to {kind} backend; {selected} selected"
        return None, "attached"
    if not lease_available(state):
        return None, "waiting for runtime owner"
    if selected == "rust":
        # The Rust server finds its web assets next to its executable.
        command = [str(rust_server(config)), "--port", str(config["port"])]
    else:
        resources = Path(config["resources"])
        script = backend_scripts(resources) / "codex-canvas"
        if not script.is_file() or not (renderer_dist(resources) / "index.html").is_file():
            raise RuntimeError("The installed backend or web assets are unavailable.")
        command = [config["python"], "-B", str(script), "--port", str(config["port"])]
    env = launch_environment(config, state, supervisor_fallback=supervisor_fallback)
    with (state / "canvas.log").open("ab", buffering=0) as log:
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


def pending_update_name(resources):
    """The staged-update file name from the packaged update module, or None without it."""
    scripts = backend_scripts(resources).resolve()
    sys.path.insert(0, str(scripts))
    try:
        import codex_peer_updates
        return codex_peer_updates.PENDING_FILE
    except ImportError:
        return None
    finally:
        sys.path.remove(str(scripts))


def update_tick(filename, config, state, run=subprocess.run):
    """Run the installer of this app when the backend staged an update from a paired server."""
    name = pending_update_name(config["resources"])
    if name is None or not (state / name).is_file():
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
            except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
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


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", type=Path, required=True)
    args = parser.parse_args()
    run(args.config.resolve())
