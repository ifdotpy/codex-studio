"""Sign-in links of native chats on this server (docs/vm-browser-bridge.md, "Native chats").

A native Codex or Claude chat runs as the user of this server, so the peer UID that separates
VM chats cannot name its chat. The chat's session environment names it instead
(`STUDIO_BROWSER_SESSION`: its agent ID or native thread ID). That is a hint, not a security
boundary: native agents of one user can already act for each other, and the person's approval
of the card is the consent. The guest's BrowserBridge does the rest here unchanged: the URL
rules, the first hop of a loopback start page, the queue, the lease and the one relayed GET.
"""
from __future__ import annotations

import asyncio
import getpass
import json
import os
from pathlib import Path
import platform
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
from typing import Any, Awaitable, Callable
import uuid

from codex_layout import VM_GUEST_ROOT
from codex_linux_vm import LinuxVMError

if str(VM_GUEST_ROOT) not in sys.path:
    sys.path.append(str(VM_GUEST_ROOT))

from browser_bridge import BridgeError, BrowserBridge, Listener, proc_listeners  # noqa: E402

FOLDER = "browser-host"
SESSION_FILE = "studio-browser.json"
HOST_BROWSER_CONTROL_SOCKET_NAME = "control.sock"
LAUNCHER = "studio-browser"
# The folder of launcher links for tools that search PATH instead of reading BROWSER. The Rust
# backend (runtime-services browser_host.rs) writes the same folder and names.
PATH_FOLDER = "bin"
PATH_NAMES = ("xdg-open", "x-www-browser", "www-browser")


def lsof_listeners(text: str) -> list[Listener]:
    """The TCP listeners in `lsof -F ptun` output, with their owner UIDs."""
    rows: list[Listener] = []
    uid: int | None = None
    family = ""
    for line in text.splitlines():
        tag, value = line[:1], line[1:]
        if tag == "p":
            uid, family = None, ""
        elif tag == "u":
            uid = int(value) if value.isdigit() else None
        elif tag == "t":
            family = value
        elif tag == "n" and uid is not None:
            host, _, port = value.rpartition(":")
            if not port.isdigit():
                continue
            host = host.strip("[]")
            if host == "*":
                host = "::" if family == "IPv6" else "0.0.0.0"
            rows.append(Listener(host, int(port), uid))
    return rows


def host_listeners() -> list[Listener]:
    if platform.system() == "Linux":
        return proc_listeners()
    try:
        result = subprocess.run(["lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-F", "ptun"],
                                capture_output=True, text=True, timeout=10, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return []
    return lsof_listeners(result.stdout)


def chat(runtime: Any, session: str) -> dict[str, Any]:
    """The native chat that a session hint names."""
    with runtime.db() as db:
        rows = db.execute("SELECT record FROM runtime_agents WHERE id=? OR json_extract(record,'$.threadId')=? "
                          "LIMIT 2", (session, session)).fetchall()
    if len(rows) != 1:
        raise ValueError("Unknown chat")
    agent: dict[str, Any] = json.loads(rows[0][0])
    if agent.get("deletedAt") or agent.get("executionMode") == "vm":
        raise ValueError("Not a native chat")
    return {"uid": os.getuid(), "projectId": agent.get("layrProjectId"), "owner": getpass.getuser(),
            "agentId": agent["id"]}


def context(runtime: Any) -> Callable[[str], Awaitable[dict[str, Any]]]:
    async def lookup(session: str) -> dict[str, Any]:
        return await asyncio.to_thread(chat, runtime, session)
    return lookup


def _write(path: Path, text: str, mode: int) -> None:
    if path.exists() and path.read_text(encoding="utf-8") == text:
        return
    descriptor, temporary = tempfile.mkstemp(prefix=".studio-browser-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            output.write(text)
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def session_environment(state: Path) -> dict[str, str]:
    """The environment that sends a native chat's browser links to this server. The launcher
    runs the guest's shim with this server's Python, from a folder without spaces, because
    tools split BROWSER into words."""
    folder = Path(state) / FOLDER
    folder.mkdir(mode=0o700, parents=True, exist_ok=True)
    folder.chmod(0o700)
    shim = folder / LAUNCHER
    _write(shim, "#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I "
           + shlex.quote(str(VM_GUEST_ROOT / "studio_browser.py")) + ' "$@"\n', 0o755)
    links = folder / PATH_FOLDER
    links.mkdir(mode=0o700, exist_ok=True)
    for name in PATH_NAMES:
        _link(links / name, Path("..") / LAUNCHER)
    return {"BROWSER": str(shim), "STUDIO_BROWSER_SOCKET": str(folder / "open.sock"),
            "STUDIO_BROWSER_PATH": str(links)}


def _link(path: Path, target: Path) -> None:
    if path.is_symlink() and Path(os.readlink(path)) == target:
        return
    temporary = path.with_name("." + path.name + "-" + uuid.uuid4().hex[:8])
    temporary.symlink_to(target)
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def thread_environment(state: Path, agent_id: str) -> dict[str, str]:
    """Per-thread Codex config overrides that send a host Codex chat's browser links to this
    server (docs/vm-browser-bridge.md, section 6.4). Codex applies `shell_environment_policy.set`
    to the commands of the thread; its app-server launch stays unchanged. PATH is not set: a
    Codex `set.PATH` replaces the whole PATH of the command. Nothing is set before the launcher
    exists, so a server without the host bridge keeps the tool's own browser behaviour."""
    folder = Path(state) / FOLDER
    launcher = folder / LAUNCHER
    if not launcher.is_file():
        return {}
    return {"shell_environment_policy.set.BROWSER": str(launcher),
            "shell_environment_policy.set.STUDIO_BROWSER_SOCKET": str(folder / "open.sock"),
            "shell_environment_policy.set.STUDIO_BROWSER_SESSION": agent_id}


def publish(root: Path, state: Path) -> None:
    """Give a Claude bridge the session environment. The bridge reads this file when it starts
    a session; its own process environment stays unchanged, so its launch identity does too."""
    _write(Path(root) / SESSION_FILE, json.dumps(session_environment(state), sort_keys=True) + "\n", 0o600)


class HostGuest:
    """The guest JSON-RPC methods of a BrowserBridge that runs in this server process."""

    def __init__(self, runtime: Any, *, listeners: Callable[[], list[Listener]] = host_listeners,
                 http_providers: frozenset[str] = frozenset()) -> None:
        environment = session_environment(Path(runtime.root))
        self.socket = Path(environment["STUDIO_BROWSER_SOCKET"])
        self.control_socket = self.socket.parent / HOST_BROWSER_CONTROL_SOCKET_NAME
        self.control_tasks: set[asyncio.Task[Any]] = set()
        self.closing = threading.Event()
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(target=self.loop.run_forever, name="studio-browser-host", daemon=True)
        self.thread.start()

        async def start() -> BrowserBridge:
            bridge = BrowserBridge(self.socket.parent, context(runtime), listeners=listeners,
                                   http_providers=http_providers)
            await bridge.listen(self.socket)
            # The folder is private; only this user's processes reach the socket.
            self.socket.chmod(0o600)
            self.control_socket.unlink(missing_ok=True)
            self.control_server = await asyncio.start_unix_server(
                self.control_client, path=str(self.control_socket), limit=64 * 1024
            )
            self.control_socket.chmod(0o600)
            return bridge
        try:
            self.bridge = asyncio.run_coroutine_threadsafe(start(), self.loop).result(10)
        except BaseException:
            self.loop.call_soon_threadsafe(self.loop.stop)
            raise

    def call(self, method: str, params: dict[str, Any] | None = None, *,
             request_id: str | None = None, timeout: float = 60) -> Any:
        identity = request_id or str(uuid.uuid4())
        payload = json.dumps({
            "id": identity,
            "method": method,
            "params": params or {},
        }, separators=(",", ":")).encode() + b"\n"
        if len(payload) > 64 * 1024:
            raise LinuxVMError("The browser request is too large", code="invalid_request")
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(timeout)
                client.connect(str(self.control_socket))
                client.sendall(payload)
                response = bytearray()
                while len(response) <= 64 * 1024:
                    chunk = client.recv(min(4096, 64 * 1024 + 1 - len(response)))
                    if not chunk:
                        break
                    response.extend(chunk)
                    if b"\n" in chunk:
                        break
            if len(response) > 64 * 1024 or not response.endswith(b"\n"):
                raise LinuxVMError("The browser control response is invalid", code="invalid_response")
            value = json.loads(response[:-1])
            if not isinstance(value, dict) or value.get("id") != identity:
                raise LinuxVMError("The browser control response is invalid", code="invalid_response")
            error = value.get("error")
            if isinstance(error, dict):
                raise LinuxVMError(
                    str(error.get("message") or "The browser request failed"),
                    code=str(error.get("code") or "unavailable"),
                )
            if "result" not in value:
                raise LinuxVMError("The browser control response is invalid", code="invalid_response")
            return value["result"]
        except (OSError, TimeoutError) as error:
            raise LinuxVMError("The host browser control socket is unavailable", code="unavailable") from error

    async def control_client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        if self.closing.is_set():
            # close() cancels only the tasks it can see. A connection accepted before the
            # control server closed can start its handler after that snapshot; without this
            # check its browser.wait long poll keeps wait_closed() open past the close deadline.
            writer.close()
            return
        task = asyncio.current_task()
        if task is not None:
            self.control_tasks.add(task)
        identity = ""
        try:
            line = await asyncio.wait_for(reader.readline(), 5)
            if len(line) > 64 * 1024 or not line.endswith(b"\n"):
                raise BridgeError("invalid_request", "Send one bounded JSON request")
            request = json.loads(line)
            if not isinstance(request, dict):
                raise BridgeError("invalid_request", "The request must be an object")
            request_identity: object = request.get("id")
            identity = request_identity if isinstance(request_identity, str) else ""
            method = request.get("method")
            params = request.get("params", {})
            if not identity or not isinstance(method, str) or not isinstance(params, dict):
                raise BridgeError("invalid_request", "The request identity, method or parameters are invalid")
            result = await self.bridge.call(method, params)
            response = {"id": identity, "result": result}
        except BridgeError as error:
            response = {"id": identity, "error": {"code": error.code, "message": str(error)}}
        except (asyncio.TimeoutError, OSError, ValueError, TypeError):
            response = {"id": identity, "error": {"code": "invalid_request", "message": "The browser request is invalid"}}
        try:
            writer.write(json.dumps(response, separators=(",", ":")).encode() + b"\n")
            await asyncio.wait_for(writer.drain(), 5)
        except (asyncio.TimeoutError, OSError):
            pass
        finally:
            if task is not None:
                self.control_tasks.discard(task)
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass

    def close(self) -> None:
        if self.closing.is_set():
            return
        self.closing.set()
        try:
            async def shutdown() -> None:
                self.control_server.close()
                # A browser.wait RPC may be waiting on the bridge's long poll. Stop
                # accepted control calls before waiting for the Unix server to drain.
                clients = [task for task in self.control_tasks if task is not asyncio.current_task()]
                for task in clients:
                    task.cancel()
                if clients:
                    await asyncio.gather(*clients, return_exceptions=True)
                await self.control_server.wait_closed()
                await self.bridge.close()

            asyncio.run_coroutine_threadsafe(shutdown(), self.loop).result(10)
            self.control_socket.unlink(missing_ok=True)
        finally:
            self.loop.call_soon_threadsafe(self.loop.stop)
            if threading.current_thread() is not self.thread:
                self.thread.join(timeout=2)
