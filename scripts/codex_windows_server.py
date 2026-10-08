"""Run the Windows API backend with a persistent native process supervisor."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import uuid

_configured_root = os.environ.get("CODEX_STUDIO_SOURCE_DIR")
_default_root = Path(__file__).resolve().parents[1]
_module_root = Path(_configured_root).expanduser() if _configured_root else _default_root
sys.path.insert(0, str(_module_root / "scripts"))

from codex_private_paths import ensure_private_dir
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
    backend_script = root / "scripts" / "codex_windows_backend.py"
    stopping = threading.Event()
    previous_handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
    for sig in previous_handlers:
        signal.signal(sig, lambda _signum, _frame: stopping.set())

    supervisor: subprocess.Popen[bytes] | None = None
    backend: subprocess.Popen[bytes] | None = None
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
            if supervisor is not None and supervisor.poll() is not None:
                raise RuntimeError("The native process supervisor exited")
            env["CODEX_AGENTS_BACKEND_ID"] = str(uuid.uuid4())
            with backend_log_path.open("ab", buffering=0) as output:
                command = [sys.executable, "-B", str(backend_script), "--port", str(port)]
                output.write((json.dumps({"event": "backend_start", "command": command}, ensure_ascii=True)
                              + "\n").encode("utf-8"))
                backend = subprocess.Popen(
                    command,
                    cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=output,
                    stderr=subprocess.STDOUT, close_fds=True,
                )
                while not stopping.is_set() and backend.poll() is None:
                    if supervisor is not None and supervisor.poll() is not None:
                        raise RuntimeError("The native process supervisor exited")
                    time.sleep(0.25)
                output.write((json.dumps({"event": "backend_exit", "returnCode": backend.poll()})
                              + "\n").encode("utf-8"))
            if stopping.is_set():
                break
            time.sleep(delay)
            delay = min(delay * 2, 30.0)
        return 0
    finally:
        for process in (backend, supervisor):
            if process is not None and process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        for sig, handler in previous_handlers.items():
            signal.signal(sig, handler)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root")
    parser.add_argument("--state", type=Path, default=state_dir())
    parser.add_argument("--port", type=int, default=4630)
    parser.add_argument("--public-origin", default="https://kukuka-win.tailf00fa0.ts.net:8443")
    parser.add_argument("--check-config", action="store_true")
    args = parser.parse_args()
    try:
        root = _source_root(args.source_root)
        if not 1 <= args.port <= 65535:
            raise ValueError("The API port must be from 1 to 65535")
        from codex_remote import validate_origin

        origin = validate_origin(args.public_origin)
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
