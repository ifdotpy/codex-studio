"""VM-only host_exec tool and the Mac service lifecycle."""
from __future__ import annotations

import fcntl
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
from typing import Any, Iterator, Protocol


class GuestClient(Protocol):
    state_dir: Path
    def request(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
                timeout: float = 60) -> dict[str, Any]: ...
    def stream(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
               timeout: float = 60) -> Iterator[dict[str, Any]]: ...


def tool_definition() -> dict[str, Any]:
    return {"name": "host_exec", "description": (
        "Run a macOS command from your VM line. Sources return through layr. Slots keep stable paths and caches. "
        "Use HOST_EXEC_DERIVED_DATA, HOST_EXEC_PACKAGE_CACHE and HOST_EXEC_ARTIFACTS for build output. "
        "Supply the same request_id for exact retries. Lost results never repeat the command. "
        "Use status to read saved output and cancel to stop the command. Artifacts return as files."),
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["execute", "status", "cancel"]},
            "command": {"type": "string"}, "cwd": {"type": "string"},
            "timeout_seconds": {"type": "number", "minimum": 0.01, "maximum": 3600},
            "request_id": {"type": "string"}, "operation_id": {"type": "string"},
            "after_seq": {"type": "integer", "minimum": 0}}, "required": ["action"]}}


def ensure_service(state_dir: Path | str) -> None:
    state = Path(state_dir).resolve()
    state.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (state / "host-exec-start.lock").open("a+") as lease:
        fcntl.flock(lease, fcntl.LOCK_EX)
        token = state / "host-exec-token"
        if not token.exists():
            with token.open("x") as stream:
                os.chmod(token, 0o600)
                stream.write(os.urandom(32).hex())
                stream.flush()
                os.fsync(stream.fileno())
        def healthy() -> bool:
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
                    channel.settimeout(1)
                    channel.connect(str(state / "host-exec.sock"))
                    channel.sendall(json.dumps({"id": "health", "method": "health", "token": token.read_text()}).encode() + b"\n")
                    data = channel.recv(1024)
                    return bool(json.loads(data).get("result", {}).get("protocol") == 1)
            except (OSError, ValueError):
                return False
        if healthy():
            return
        with (state / "host-exec.log").open("ab") as log:
            child = subprocess.Popen([sys.executable, str(Path(__file__).with_name("codex_host_exec_server.py")), str(state)],
                stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True, close_fds=True)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if child.poll() is not None:
                raise RuntimeError("The host_exec service exited. Log: " + str(state / "host-exec.log"))
            if healthy():
                return
            time.sleep(0.05)
        raise RuntimeError("The host_exec service has no proven start result. Inspect " + str(state / "host-exec.log"))


def configure_guest(client: GuestClient) -> None:
    token = (client.state_dir / "host-exec-token").read_text()
    identity = "host-configure:" + hashlib.sha256(token.encode()).hexdigest()
    client.request("host.configure", {"token": token}, request_id=identity, timeout=15)


def run_tool(runtime: Any, actor: dict[str, Any], args: dict[str, Any], request_id: str) -> dict[str, Any]:
    if actor.get("role") == "reviewer":
        raise PermissionError("host_exec is unavailable to read-only reviewers")
    from codex_linux_workspaces import client
    root = runtime.agent(actor.get("rootId") or actor["id"])
    if root.get("executionMode") != "vm" or actor.get("executionMode", "vm") != "vm":
        raise PermissionError("host_exec is available only to VM agents")
    action = args.get("action", "execute")
    if action not in {"execute", "status", "cancel"}:
        raise ValueError("Unknown host_exec action")
    identity = args.get("request_id") or request_id
    # Provider call IDs can exceed the guest's identity limit. Hash, do not truncate.
    identity = "host-exec:" + hashlib.sha256((actor["id"] + ":" + str(identity)).encode()).hexdigest()
    params: dict[str, Any] = {"agentId": actor["id"], "action": action}
    if action == "execute":
        command = args.get("command")
        if not isinstance(command, str) or not command or "\0" in command:
            raise ValueError("Supply a command")
        timeout = args.get("timeout_seconds", 300)
        if not isinstance(timeout, (int, float)) or isinstance(timeout, bool) or not 0 < timeout <= 3600:
            raise ValueError("The command timeout must be between 0 and 3600 seconds")
        params.update(argv=["/bin/zsh", "-lc", command], cwd=args.get("cwd", actor["cwd"]),
                      timeoutSeconds=timeout, operationId=identity)
    else:
        operation = args.get("operation_id")
        if not isinstance(operation, str) or not operation:
            raise ValueError("Supply the host_exec operation_id")
        params.update(operationId=operation, afterSeq=args.get("after_seq", 0))
        timeout = 15
    remote: GuestClient = client(runtime)
    output: list[str] = []
    length = 0
    result: dict[str, Any] | None = None
    for frame in remote.stream("host.exec", params, request_id=identity, timeout=float(timeout) + 3660 if action == "execute" else 60):
        if frame.get("event") == "output":
            value = frame["data"].get("text", "")
            if length < 256 * 1024:
                output.append(value[:256 * 1024 - length])
                length += len(value)
        if "result" in frame:
            result = frame["result"]
    if result is None:
        raise RuntimeError("The host command has no proven result. Operation: " + str(params["operationId"]))
    return {**result, "output": "".join(output)}
