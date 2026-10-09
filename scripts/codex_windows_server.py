"""Run the Windows API backend with a persistent native process supervisor."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

_configured_root = os.environ.get("CODEX_STUDIO_SOURCE_DIR")
_default_root = Path(__file__).resolve().parents[1]
_module_root = Path(_configured_root).expanduser() if _configured_root else _default_root
sys.path.insert(0, str(_module_root / "scripts"))

from codex_private_paths import ensure_private_dir, protect_temp_file
from codex_state import state_dir


def _source_root(configured: str | None) -> Path:
    if configured:
        root = Path(configured).expanduser().resolve()
    else:
        root = Path(__file__).resolve().parents[1]
    scripts = root / "scripts"
    if not (scripts / "codex_canvas.py").is_file():
        raise ValueError("The source folder must contain scripts/codex_canvas.py")
    return root


def _child_env(root: Path, state: Path, origin: str) -> dict[str, str]:
    env = os.environ.copy()
    env.update({
        "CODEX_AGENTS_STATE_DIR": str(state),
        "CODEX_AGENTS_SUPERVISOR_MODE": "1",
        "CODEX_CANVAS_PUBLIC_ORIGIN": origin,
        "CODEX_STUDIO_SOURCE_DIR": str(root),
    })
    return env


def _wait_supervisor(state: Path, process: subprocess.Popen[bytes] | None,
                     timeout: float = 30) -> bool:
    from codex_process_supervisor import status

    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            return False
        try:
            value = status(state)
            identity = value.get("stateDir")
            return isinstance(identity, str) and identity == str(state.resolve())
        except (OSError, RuntimeError, ValueError):
            time.sleep(0.25)
    return False


def _control_request_path(state: Path, request_id: str) -> Path:
    return state / f"windows-server-control-request-{request_id}.json"


def _write_control_request(state: Path, action: str, source_root: str | None = None) -> str:
    if action not in {"stop-backend", "restart-backend", "stop-all"}:
        raise ValueError("Unknown Windows server control action")
    ensure_private_dir(state)
    request_id = uuid.uuid4().hex
    payload = {"requestId": request_id, "action": action}
    if source_root is not None:
        payload["sourceRoot"] = str(_source_root(source_root))
    fd, temporary_name = tempfile.mkstemp(prefix="windows-server-control-", suffix=".tmp", dir=state)
    temporary = Path(temporary_name)
    try:
        protect_temp_file(temporary)
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as stream:
            json.dump(payload, stream, separators=(",", ":"))
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, _control_request_path(state, request_id))
    except BaseException:
        try:
            os.close(fd)
        except OSError:
            pass
        temporary.unlink(missing_ok=True)
        raise
    return request_id


def _read_control_request(state: Path, previous_request: str | None) -> dict[str, str] | None:
    for request_path in sorted(state.glob("windows-server-control-request-*.json")):
        request_id = request_path.stem.removeprefix("windows-server-control-request-")
        claim_path = request_path.with_suffix(".claim")
        try:
            os.replace(request_path, claim_path)
        except FileNotFoundError:
            continue
        try:
            value = json.loads(claim_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError):
            claim_path.unlink(missing_ok=True)
            continue
        if not isinstance(value, dict):
            claim_path.unlink(missing_ok=True)
            continue
        value_id = value.get("requestId")
        action = value.get("action")
        source_root = value.get("sourceRoot")
        if (not isinstance(value_id, str) or value_id != request_id or value_id == previous_request
                or action not in {"stop-backend", "restart-backend", "stop-all"}
                or (source_root is not None and not isinstance(source_root, str))):
            claim_path.unlink(missing_ok=True)
            continue
        claim_path.unlink(missing_ok=True)
        return {"requestId": value_id, "action": action, "sourceRoot": source_root or ""}
    return None


def _write_control_result(state: Path, request_id: str, result: str) -> None:
    path = state / f"windows-server-control-{request_id}.json"
    fd, temporary_name = tempfile.mkstemp(prefix="windows-server-result-", suffix=".tmp", dir=state)
    temporary = Path(temporary_name)
    protect_temp_file(temporary)
    with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as stream:
        json.dump({"requestId": request_id, "result": result}, stream, separators=(",", ":"))
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def _stop_backend(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        if os.name == "nt" and hasattr(signal, "CTRL_BREAK_EVENT"):
            process.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            process.send_signal(signal.SIGTERM)
        process.wait(timeout=70)
        return
    except (OSError, subprocess.TimeoutExpired):
        pass
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def _stop_supervisor(state: Path) -> None:
    lease = state / "supervisor.lock"
    identity = json.loads(lease.read_text(encoding="utf-8"))
    pid, creation_time = identity.get("pid"), identity.get("startTime")
    if type(pid) is not int or not isinstance(creation_time, str):
        raise RuntimeError("The supervisor lease has no verifiable process identity")
    from codex_process_supervisor import process_start_time

    current = process_start_time(pid)
    if current is None:
        return
    if current != creation_time:
        raise RuntimeError("The supervisor process identity changed; refusing to stop it")
    from codex_windows_supervisor import terminate_process_identity

    terminate_process_identity(pid, creation_time)


def run(root: Path, state: Path, port: int, origin: str) -> int:
    if os.name != "nt":
        raise RuntimeError("The native Windows server entrypoint runs on Windows only")
    from codex_remote import validate_origin

    origin = validate_origin(origin)
    ensure_private_dir(state)
    os.environ.update(_child_env(root, state, origin))
    sys.path.insert(0, str(root / "scripts"))
    from codex_process_supervisor import status

    env = _child_env(root, state, origin)
    supervisor_log_path = state / "supervisor.log"
    backend_log_path = state / "backend.log"
    supervisor_script = root / "scripts" / "codex_process_supervisor.py"
    stopping = threading.Event()
    previous_handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
    for sig in previous_handlers:
        signal.signal(sig, lambda _signum, _frame: stopping.set())

    supervisor: subprocess.Popen[bytes] | None = None
    backend: subprocess.Popen[bytes] | None = None
    backend_enabled = True
    handled_request: str | None = None
    pending_request: dict[str, str] | None = None
    stop_all_request: str | None = None
    try:
        try:
            status(state)
        except (OSError, RuntimeError, ValueError):
            with supervisor_log_path.open("ab", buffering=0) as output:
                supervisor = subprocess.Popen(
                    [sys.executable, "-B", str(supervisor_script), "--state", str(state)],
                    cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=output,
                    stderr=subprocess.STDOUT, close_fds=True,
                )
            if not _wait_supervisor(state, supervisor):
                raise RuntimeError("The native process supervisor did not become ready")

        delay = 2.0
        while not stopping.is_set():
            request = pending_request or _read_control_request(state, handled_request)
            pending_request = None
            if request is not None:
                handled_request = request["requestId"]
                if request["action"] == "stop-all":
                    stop_all_request = handled_request
                    stopping.set()
                    break
                if request["sourceRoot"]:
                    root = _source_root(request["sourceRoot"])
                    env = _child_env(root, state, origin)
                    os.environ.update(env)
                    sys.path.insert(0, str(root / "scripts"))
                if backend is not None and backend.poll() is None:
                    _stop_backend(backend)
                if backend is not None:
                    with backend_log_path.open("ab", buffering=0) as output:
                        output.write((json.dumps({"event": "backend_exit", "returnCode": backend.poll()})
                                      + "\n").encode("utf-8"))
                backend_enabled = request["action"] != "stop-backend"
                result = "backend-stopped" if not backend_enabled else "backend-restarted"
                _write_control_result(state, handled_request, result)
                delay = 0.0
                continue
            if not backend_enabled:
                time.sleep(0.25)
                continue
            if supervisor is not None and supervisor.poll() is not None:
                raise RuntimeError("The native process supervisor exited")
            env["CODEX_AGENTS_BACKEND_ID"] = str(uuid.uuid4())
            with backend_log_path.open("ab", buffering=0) as output:
                backend_script = root / "scripts" / "codex_windows_backend.py"
                command = [sys.executable, "-B", str(backend_script), "--port", str(port)]
                output.write((json.dumps({"event": "backend_start", "command": command}, ensure_ascii=True)
                              + "\n").encode("utf-8"))
                backend = subprocess.Popen(
                    command,
                    cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=output,
                    stderr=subprocess.STDOUT, close_fds=True,
                    creationflags=getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
                    if os.name == "nt" else 0,
                )
                while not stopping.is_set() and backend.poll() is None:
                    pending_request = _read_control_request(state, handled_request)
                    if pending_request is not None:
                        break
                    if supervisor is not None and supervisor.poll() is not None:
                        raise RuntimeError("The native process supervisor exited")
                    time.sleep(0.25)
                if pending_request is None:
                    output.write((json.dumps({"event": "backend_exit", "returnCode": backend.poll()})
                                  + "\n").encode("utf-8"))
            if stopping.is_set():
                break
            if pending_request is not None:
                continue
            time.sleep(delay)
            delay = min(delay * 2, 30.0)
        return 0
    finally:
        if backend is not None and backend.poll() is None:
            _stop_backend(backend)
        if stop_all_request:
            _stop_supervisor(state)
            if supervisor is not None:
                supervisor.wait(timeout=5)
        elif supervisor is not None and supervisor.poll() is None:
            _stop_supervisor(state)
            supervisor.wait(timeout=5)
        if backend is not None and stop_all_request:
            with backend_log_path.open("ab", buffering=0) as output:
                output.write((json.dumps({"event": "backend_exit", "returnCode": backend.poll()})
                              + "\n").encode("utf-8"))
        if stop_all_request:
            _write_control_result(state, stop_all_request, "stopped-all")
        for sig, handler in previous_handlers.items():
            signal.signal(sig, handler)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root")
    parser.add_argument("--state", type=Path, default=state_dir())
    parser.add_argument("--port", type=int, default=4630)
    parser.add_argument("--public-origin", default="https://kukuka-win.tailf00fa0.ts.net:8443")
    parser.add_argument("--check-config", action="store_true")
    parser.add_argument("--request-action", choices=("stop-backend", "restart-backend", "stop-all"))
    parser.add_argument("--request-source-root")
    args = parser.parse_args()
    try:
        root = _source_root(args.source_root)
        if not 1 <= args.port <= 65535:
            raise ValueError("The API port must be from 1 to 65535")
        from codex_remote import validate_origin

        origin = validate_origin(args.public_origin)
        if args.request_action:
            request_id = _write_control_request(args.state.expanduser().resolve(),
                                                args.request_action, args.request_source_root)
            print(json.dumps({"requestId": request_id}, separators=(",", ":")))
            return 0
        if args.check_config:
            print(json.dumps({"sourceRoot": str(root), "stateDir": str(args.state.resolve()),
                              "port": args.port, "publicOrigin": origin}, separators=(",", ":")))
            return 0
        return run(root, args.state.expanduser().resolve(), args.port, origin)
    except (OSError, RuntimeError, ValueError) as error:
        parser.exit(1, f"codex-windows-server: {error}\n")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
