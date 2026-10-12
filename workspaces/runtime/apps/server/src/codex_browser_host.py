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
import subprocess
import sys
import tempfile
import threading
from typing import Any, Awaitable, Callable

from codex_layout import VM_GUEST_ROOT
from codex_linux_vm import LinuxVMError

if str(VM_GUEST_ROOT) not in sys.path:
    sys.path.append(str(VM_GUEST_ROOT))

from browser_bridge import BridgeError, BrowserBridge, Listener, proc_listeners  # noqa: E402

FOLDER = "browser-host"
SESSION_FILE = "studio-browser.json"


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
    shim = folder / "studio-browser"
    _write(shim, "#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I "
           + shlex.quote(str(VM_GUEST_ROOT / "studio_browser.py")) + ' "$@"\n', 0o755)
    return {"BROWSER": str(shim), "STUDIO_BROWSER_SOCKET": str(folder / "open.sock")}


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
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(target=self.loop.run_forever, name="studio-browser-host", daemon=True)
        self.thread.start()

        async def start() -> BrowserBridge:
            bridge = BrowserBridge(self.socket.parent, context(runtime), listeners=listeners,
                                   http_providers=http_providers)
            await bridge.listen(self.socket)
            # The folder is private; only this user's processes reach the socket.
            self.socket.chmod(0o600)
            return bridge
        try:
            self.bridge = asyncio.run_coroutine_threadsafe(start(), self.loop).result(10)
        except BaseException:
            self.loop.call_soon_threadsafe(self.loop.stop)
            raise

    def call(self, method: str, params: dict[str, Any] | None = None, *,
             request_id: str | None = None, timeout: float = 60) -> Any:
        future = asyncio.run_coroutine_threadsafe(self.bridge.call(method, params or {}), self.loop)
        try:
            return future.result(timeout)
        except BridgeError as error:
            raise LinuxVMError(str(error), code=error.code) from None

    def close(self) -> None:
        try:
            asyncio.run_coroutine_threadsafe(self.bridge.close(), self.loop).result(10)
        finally:
            self.loop.call_soon_threadsafe(self.loop.stop)
