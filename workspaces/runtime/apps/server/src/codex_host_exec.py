"""VM-only host_exec tool and the Mac service lifecycle."""
from __future__ import annotations

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
from typing import Any, Iterator, Protocol

from codex_layout import VM_GUEST_ROOT


class GuestClient(Protocol):
    state_dir: Path
    def request(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
                timeout: float = 60) -> dict[str, Any]: ...
    def stream(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
               timeout: float = 60) -> Iterator[dict[str, Any]]: ...


def tool_definition() -> dict[str, Any]:
    # The same canonical format as TOOLS: the app-server rejects a mixed list.
    return {"type": "function", "name": "host_exec", "description": (
        "Run a macOS command from your VM line. Sources return through layr. Slots keep stable paths and caches. "
        "Use HOST_EXEC_DERIVED_DATA, HOST_EXEC_PACKAGE_CACHE and HOST_EXEC_ARTIFACTS for build output. "
        "The line's layr cache folders go to the Mac, but Mac changes inside them never return. "
        "Supply the same request_id for exact retries. Lost results never repeat the command. "
        "Use status to read saved output and cancel to stop the command. Artifacts return as files."),
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["execute", "status", "cancel"]},
            "command": {"type": "string"}, "cwd": {"type": "string"},
            "timeout_seconds": {"type": "number", "minimum": 0.01, "maximum": 3600},
            "request_id": {"type": "string"}, "operation_id": {"type": "string"},
            "after_seq": {"type": "integer", "minimum": 0}}, "required": ["action"]}}


def service_build_id() -> str:
    """The code build of the Mac service: the hash of the files that it runs."""
    digest = hashlib.sha256()
    for path in (Path(__file__).with_name("codex_host_exec.py"), Path(__file__).with_name("codex_host_exec_server.py"),
                 Path(__file__).with_name("codex_layout.py"), VM_GUEST_ROOT / "host_exec_protocol.py"):
        digest.update(path.name.encode() + b"\0" + path.read_bytes() + b"\0")
    return digest.hexdigest()


class ServiceError(RuntimeError):
    def __init__(self, code: str, message: str):
        super().__init__("host_exec service: " + message)
        self.code = code


def _service_call(state: Path, token: str, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    identity = "host-service:" + hashlib.sha256(json.dumps([method, params], sort_keys=True).encode()).hexdigest()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
        channel.settimeout(2)
        channel.connect(str(state / "host-exec.sock"))
        channel.sendall(json.dumps({"id": identity, "method": method, "params": params or {}, "token": token}).encode() + b"\n")
        with channel.makefile("rb") as stream:
            line = stream.readline(8193)
    if not line or len(line) > 8192:
        raise ServiceError("outcome_unknown", "the response is missing or too large")
    response = json.loads(line)
    if response.get("id") != identity:
        raise ServiceError("outcome_unknown", "the response identity differs")
    if "error" in response:
        raise ServiceError(str(response["error"].get("code")), str(response["error"].get("message")))
    return dict(response["result"])


def _owner_exited(state: Path) -> bool:
    with (state / "host-exec.lock").open("a+") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        return True


def _legacy_service_pid(state: Path) -> int | None:
    """The process of a service without a build identity, from its socket (macOS LOCAL_PEERPID).
    It counts only when its command line is the service of this state folder."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
        channel.settimeout(2)
        channel.connect(str(state / "host-exec.sock"))
        pid = int(channel.getsockopt(0, 0x002))
    command = subprocess.run(["ps", "-ww", "-o", "command=", "-p", str(pid)], capture_output=True, text=True,
                             timeout=10).stdout
    if "codex_host_exec_server.py" not in command or str(state) not in command:
        return None
    return pid


def _stop_old_service(state: Path, token: str, health: dict[str, Any]) -> str | None:
    """Stop an idle service of another build. Return why a busy service stays."""
    if health.get("buildId"):
        try:
            _service_call(state, token, "service.retire", {"buildId": health["buildId"], "pid": health["pid"]})
        except ServiceError as exc:
            if exc.code == "busy":
                return "it has a running command or request"
            raise
    else:
        # A service from before build identities has no retire request. A running command is its
        # child process; a lost request keeps its pending receipt and is never repeated.
        pid = _legacy_service_pid(state)
        if pid is None:
            return "its process is not the service of this state folder"
        children = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True, timeout=10).stdout
        if children.strip():
            return "it has a running command"
        os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    while not _owner_exited(state):
        if time.monotonic() >= deadline:
            raise RuntimeError("The old host_exec service has no proven stop result")
        time.sleep(0.05)
    return None


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
        # A running service keeps its code after an app update. An idle service of another
        # build is replaced; a busy one stays until a later start (docs/host-exec.md).
        build = service_build_id()
        def probe() -> dict[str, Any] | None:
            try:
                result = _service_call(state, token.read_text(), "health")
            except (OSError, ValueError, ServiceError):
                return None
            return result if result.get("protocol") == 1 else None
        current = probe()
        if current is not None:
            if current.get("buildId") == build:
                return
            kept = _stop_old_service(state, token.read_text(), current)
            if kept is not None:
                print("host_exec: the service of an older build stays because " + kept, file=sys.stderr, flush=True)
                return
        with (state / "host-exec.log").open("ab") as log:
            child = subprocess.Popen([sys.executable, str(Path(__file__).with_name("codex_host_exec_server.py")), str(state)],
                stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True, close_fds=True)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if child.poll() is not None:
                raise RuntimeError("The host_exec service exited. Log: " + str(state / "host-exec.log"))
            current = probe()
            if current is not None and current.get("buildId") == build:
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
    # The Mac service may have stopped since the VM started (only a VM start ensured it before);
    # a check costs one local request.
    ensure_service(remote.state_dir)
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
