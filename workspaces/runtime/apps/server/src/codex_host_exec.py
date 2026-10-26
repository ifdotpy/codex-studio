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
import threading
import time
from typing import TYPE_CHECKING, Any, Iterator, Protocol

from codex_layout import VM_GUEST_ROOT

if TYPE_CHECKING:
    from codex_time import ToolResult


class GuestClient(Protocol):
    state_dir: Path
    def request(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
                timeout: float = 60) -> dict[str, Any]: ...
    def stream(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
               timeout: float = 60) -> Iterator[dict[str, Any]]: ...


# Environment names of the Mac command (docs/host-exec.md); the Mac service sets them.
ENV_DERIVED_DATA = "HOST_EXEC_DERIVED_DATA"
ENV_PACKAGE_CACHE = "HOST_EXEC_PACKAGE_CACHE"
ENV_ARTIFACTS = "HOST_EXEC_ARTIFACTS"
# The ID of the layr state that the slot holds.
ENV_STATE = "HOST_EXEC_STATE"
# The git tree ID of that state: what `layr rev-parse <state>^{tree}` prints, 40 hex digits.
ENV_TREE = "HOST_EXEC_TREE"
# The host_exec tool actions; the guest broker implements each (vm-guest/host_exec.py).
ACTIONS = ("execute", "status", "cancel", "list", "release")
# Seconds between two resume attempts of one interrupted receipt while the VM gives no answer.
RESUME_RETRY_SECONDS = 30
# Resume attempts of one interrupted receipt without a guest answer. After the last one the
# saved "outcome unknown" answer goes to the waiting caller (codex_tool_response_recovery).
RESUME_ATTEMPTS = 10
# Seconds between two scans for interrupted receipts while a scan still found one to resume.
RESUME_SCAN_SECONDS = 10
RESUME_NOTE = ("Studio restarted while this call ran. This is the saved guest result of the same request; "
               "nothing was repeated. The output stays readable with action status and this operationId.")


def tool_definition() -> dict[str, Any]:
    # The same canonical format as TOOLS: the app-server rejects a mixed list.
    return {"type": "function", "name": "host_exec", "description": (
        "Run a macOS command from your VM line. Sources return through layr. Slots keep stable paths and caches. "
        f"Use {ENV_DERIVED_DATA}, {ENV_PACKAGE_CACHE} and {ENV_ARTIFACTS} for build output. "
        f"{ENV_STATE} names the layr state in the slot and {ENV_TREE} its git tree ID. "
        "The line's layr cache folders go to the Mac, but Mac changes inside them never return. "
        "Supply the same request_id for exact retries. Lost results never repeat the command. "
        "Use status to read saved output and cancel to stop the command. Artifacts return as files. "
        "Use list to find your operations, also after a lost result, and release to free the slot of "
        "an exited command whose result you will not collect; its saved output stays readable."),
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": list(ACTIONS)},
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
            child = subprocess.Popen([sys.executable, "-B", str(Path(__file__).with_name("codex_host_exec_server.py")), str(state)],
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
    from codex_linux_vm import LinuxVMError
    root = runtime.agent(actor.get("rootId") or actor["id"])
    if root.get("executionMode") != "vm" or actor.get("executionMode", "vm") != "vm":
        raise PermissionError("host_exec is available only to VM agents")
    action = args.get("action", "execute")
    if action not in ACTIONS:
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
    elif action == "list":
        timeout = 15
    else:
        operation = args.get("operation_id")
        if not isinstance(operation, str) or not operation:
            raise ValueError("Supply the host_exec operation_id")
        params.update(operationId=operation, afterSeq=args.get("after_seq", 0))
        timeout = 15
    stream_timeout = float(timeout) + 3660 if action == "execute" else 60
    remember_request(runtime, request_id, {"requestId": identity, "params": params, "timeout": stream_timeout})
    remote: GuestClient = client(runtime)
    # The Mac service may have stopped since the VM started (only a VM start ensured it before);
    # a check costs one local request.
    ensure_service(remote.state_dir)
    output: list[str] = []
    length = 0
    result: dict[str, Any] | None = None
    operation_note = ("Operation: " + str(params["operationId"]) + ". Use action status or list; never repeat the command."
                      if "operationId" in params else "")
    try:
        for frame in remote.stream("host.exec", params, request_id=identity, timeout=stream_timeout):
            if frame.get("event") == "output":
                value = frame["data"].get("text", "")
                if length < 256 * 1024:
                    output.append(value[:256 * 1024 - length])
                    length += len(value)
            if "result" in frame:
                result = frame["result"]
    except LinuxVMError as exc:
        if not operation_note:
            raise
        # The agent needs the operation to find its outcome. The error keeps its uncertainty.
        raise LinuxVMError(str(exc) + " " + operation_note, uncertain=exc.uncertain, code=exc.code,
                           detail=exc.detail) from exc
    if result is None:
        raise RuntimeError("The host command has no proven result. " + operation_note)
    return {**result, "output": "".join(output)}


def remember_request(runtime: Any, key: str, request: dict[str, Any]) -> None:
    """Save the exact guest request on its running tool receipt before dispatch. The guest keeps a
    durable receipt of each host.exec request ID: after a Studio restart the same request returns
    the saved result or joins the running task, and it never runs the command again."""
    if not callable(getattr(runtime, "tool_request", None)):
        return
    with runtime.lock, runtime.db() as db:
        record = runtime.tool_request(key, db)
        if record is None or record.get("stage") != "running" or record.get("hostExecRequest") == request:
            return
        record["hostExecRequest"] = request
        runtime.put(db, "tool_requests", record)


def resume_pending(record: Any) -> bool:
    """An interrupted host_exec receipt whose saved guest request can still give the real result.
    Its saved "outcome unknown" answer waits until the resume settles or gives up."""
    return bool(isinstance(record, dict) and record.get("tool") == "host_exec"
                and record.get("stage") == "interrupted" and isinstance(record.get("hostExecRequest"), dict)
                and (record.get("hostExecResume") or {}).get("state") != "failed")


def _current(runtime: Any, db: Any, key: str, raw: str) -> bool:
    row = db.execute("SELECT record FROM runtime_tool_requests WHERE id=?", (key,)).fetchone()
    return bool(row and row[0] == raw and not runtime.closed)


def _settle(runtime: Any, key: str, raw: str, answer: "ToolResult") -> bool:
    from codex_payloads import externalize_result
    from codex_time import stamp_tool_result
    result = stamp_tool_result(answer, time.time())
    with runtime.lock, runtime.db() as db:
        if not _current(runtime, db, key, raw):
            return False
        db.execute("INSERT OR IGNORE INTO runtime_tool_results VALUES (?,?)",
                   (key, json.dumps(externalize_result(runtime.root, db, result))))
        saved = runtime.tool_result(db, key)
        runtime.finish_tool_request(key, saved if isinstance(saved, dict) else result, db=db)
    return True


def _postpone(runtime: Any, key: str, raw: str, error: BaseException) -> None:
    with runtime.lock, runtime.db() as db:
        if not _current(runtime, db, key, raw):
            return
        record = runtime.tool_request(key, db)
        attempts = int((record.get("hostExecResume") or {}).get("attempts", 0)) + 1
        record["hostExecResume"] = {"state": "failed" if attempts >= RESUME_ATTEMPTS else "retry",
                                    "attempts": attempts, "at": time.time(),
                                    "nextAt": time.time() + RESUME_RETRY_SECONDS,
                                    "error": (type(error).__name__ + ": " + str(error))[:500]}
        runtime.put(db, "tool_requests", record)


def resume(runtime: Any, key: str) -> str:
    """Settle one interrupted host_exec receipt from the guest receipt of the same request."""
    from codex_linux_vm import LinuxVMError
    from codex_linux_workspaces import client
    with runtime.lock, runtime.read_db() as db:
        row = db.execute("SELECT record FROM runtime_tool_requests WHERE id=?", (key,)).fetchone()
        record = runtime.tool_request(key, db) if row else None
    if record is None or not resume_pending(record) or record.get("nativeDelivery"):
        return "superseded"
    raw, request = row[0], record["hostExecRequest"]
    operation = request["params"].get("operationId")
    note = RESUME_NOTE + (" Operation: " + str(operation) + "." if operation else "")
    try:
        remote: GuestClient = client(runtime)
        ensure_service(remote.state_dir)
        result: dict[str, Any] | None = None
        for frame in remote.stream("host.exec", request["params"], request_id=request["requestId"],
                                   timeout=float(request["timeout"])):
            if "result" in frame:
                result = frame["result"]
        if result is None:
            raise LinuxVMError("The guest returned no host_exec result.", uncertain=True)
    except LinuxVMError as error:
        if error.code is None:
            # No guest answer (the VM or its channel is down): try again later, then give up.
            _postpone(runtime, key, raw, error)
            return "postponed"
        # The guest answered this request with an explicit error, as the original call would have.
        failure: "ToolResult" = {"success": False, "contentItems": [{"type": "inputText", "text": str(error) + " " + note}]}
        return "settled" if _settle(runtime, key, raw, failure) else "superseded"
    except Exception as error:
        _postpone(runtime, key, raw, error)
        return "postponed"
    value = {**result, "output": "", "recovery": note}
    answer: "ToolResult" = {"success": True, "contentItems": [{"type": "inputText", "text": json.dumps(value, ensure_ascii=False)}]}
    return "settled" if _settle(runtime, key, raw, answer) else "superseded"


def resume_tick(runtime: Any) -> None:
    """Start one resume per interrupted host_exec receipt. A Studio restart is the only source of
    interrupted receipts, so the scans stop when none waits for a resume."""
    state = runtime.__dict__.setdefault("_host_exec_resume", {"next": 0.0, "done": False, "jobs": {}})
    now = time.monotonic()
    if state["done"] or now < state["next"] or runtime.closed:
        return
    state["next"] = now + RESUME_SCAN_SECONDS
    with runtime.lock, runtime.read_db() as db:
        rows = db.execute("SELECT id, record FROM runtime_tool_requests WHERE json_extract(record,'$.tool')='host_exec' "
                          "AND json_extract(record,'$.stage')='interrupted' "
                          "AND json_type(record,'$.hostExecRequest')='object' "
                          "AND json_extract(record,'$.nativeDelivery') IS NULL").fetchall()
    jobs = state["jobs"]
    for finished in [key for key, job in jobs.items() if not job.is_alive()]:
        jobs.pop(finished)
    waiting = [(key, json.loads(raw)) for key, raw in rows]
    waiting = [(key, record) for key, record in waiting if resume_pending(record)]
    if not waiting and not jobs:
        state["done"] = True
        return
    for key, record in waiting:
        if key in jobs or (record.get("hostExecResume") or {}).get("nextAt", 0) > time.time():
            continue
        jobs[key] = threading.Thread(target=resume, args=(runtime, key), name="studio-host-exec-resume", daemon=True)
        jobs[key].start()
