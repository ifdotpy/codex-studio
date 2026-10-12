"""The Mac side of the VM browser bridge, and the whole flow with fake sign-in tools.

The flow tests run the real guest bridge (vm-guest/browser_bridge.py) on a Unix socket, the
real shim as a subprocess, a fake provider and fake tools in the measured kubelogin and AWS
shapes (docs/vm-browser-bridge.md), and this module with a SQLite runtime stand-in. A test
HTTP client takes the place of the browser. The tool and the forward share this host, so
the forward binds another port and the client sends the redirect there.
"""
from __future__ import annotations

import asyncio
import base64
from collections.abc import Iterator
from contextlib import contextmanager, redirect_stderr
import gzip
import http.client
import io
import json
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
from typing import Any
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request
from urllib.parse import urlsplit
import uuid
from server_test_waits import wait_for_condition as wait_until

import codex_browser_bridge as bridge_module
from codex_browser_bridge import (REMOTE_MESSAGE, SANDBOX, Bridge, Forward, PortBusy, card_id,
                                  cleaned)
from codex_layout import VM_GUEST_ROOT
from codex_linux_vm import LinuxVMError

from browser_bridge import BridgeError, BrowserBridge, Listener
from test_browser_fakes import FakeProvider, FakeTool, free_port, install_shim

UID = os.getuid()


class Runtime:
    """The runtime parts that the bridge uses: requests, agents and chat events in SQLite."""

    def __init__(self, path: Path) -> None:
        self.lock = threading.RLock()
        self.closed = False
        self.path = path
        self.events: list[tuple[str, str, str, str]] = []
        with self.db() as db:
            db.execute("CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT)")
            db.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT)")

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(self.path)
        try:
            with connection:
                yield connection
        finally:
            connection.close()

    def put(self, db: sqlite3.Connection, table: str, record: dict[str, Any]) -> None:
        db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)", (record["id"], json.dumps(record)))

    def agent(self, key: str, db: sqlite3.Connection) -> dict[str, Any]:
        row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        if not row:
            raise ValueError("Unknown managed agent")
        value: dict[str, Any] = json.loads(row[0])
        return value

    def enqueue(self, db: sqlite3.Connection, agent: dict[str, Any], kind: str, text: str, key: str) -> str:
        self.events.append((agent["id"], kind, text, key))
        return key

    def cards(self) -> list[dict[str, Any]]:
        with self.db() as db:
            return [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_requests")]


class LoopGuest:
    """The guest JSON-RPC methods of a real BrowserBridge, called from server threads."""

    def __init__(self, bridge: BrowserBridge, loop: asyncio.AbstractEventLoop) -> None:
        self.bridge, self.loop = bridge, loop
        self.methods: list[str] = []

    def call(self, method: str, params: dict[str, Any] | None = None, *,
             request_id: str | None = None, timeout: float = 60) -> Any:
        self.methods.append(method)
        future = asyncio.run_coroutine_threadsafe(self.bridge.call(method, params or {}), self.loop)
        try:
            return future.result(timeout)
        except BridgeError as error:
            raise LinuxVMError(str(error), code=error.code) from None


def provider_redirect(url: str) -> str:
    """What the browser gets from the provider: the redirect to the loopback with a code."""
    class Stop(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args: Any, **kwargs: Any) -> None:
            return None
    try:
        urllib.request.build_opener(Stop).open(url, timeout=5)
    except urllib.error.HTTPError as error:
        with error:
            return str(error.headers["Location"])
    raise AssertionError("The provider did not redirect")


def browser_get(port: int, target: str, headers: dict[str, str] | None = None,
                method: str = "GET") -> tuple[int, dict[str, str], bytes]:
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request(method, target, headers=headers or {})
        response = connection.getresponse()
        return response.status, {name.lower(): value for name, value in response.getheaders()}, response.read()
    finally:
        connection.close()


class Flow(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="sbb-", dir="/tmp"))
        self.provider = FakeProvider()
        self.tools: list[FakeTool] = []
        self.agent_id = str(uuid.uuid4())
        self.runtime = Runtime(self.root / "runtime.sqlite3")
        with self.runtime.db() as db:
            self.runtime.put(db, "agents", {"id": self.agent_id, "executionMode": "vm", "layrProjectId": "p1",
                                            "epoch": 0, "autoWake": True})
        self.loop = asyncio.new_event_loop()
        self.thread = threading.Thread(target=self.loop.run_forever, daemon=True)
        self.thread.start()
        contexts = {self.agent_id: {"uid": UID, "projectId": "p1", "owner": "studio-a-test"}}

        async def context(agent: str) -> dict[str, Any]:
            return contexts[agent]

        def listeners() -> list[Listener]:
            return [Listener("127.0.0.1", tool.port, UID) for tool in self.tools]

        async def start() -> BrowserBridge:
            value = BrowserBridge(self.root / "guest", context, listeners=listeners,
                                  http_providers=frozenset({self.provider.origin}))
            await value.listen(self.root / "open.sock")
            return value
        self.guest_bridge = asyncio.run_coroutine_threadsafe(start(), self.loop).result(10)
        self.guest = LoopGuest(self.guest_bridge, self.loop)
        self.bridge = Bridge(self.runtime, lambda: self.guest, http_providers=frozenset({self.provider.origin}),
                             listen_port=lambda port: 0)
        install_shim(self.root / "bin")

    def tearDown(self) -> None:
        self.bridge.stop()
        asyncio.run_coroutine_threadsafe(self.guest_bridge.close(), self.loop).result(10)
        self.loop.call_soon_threadsafe(self.loop.stop)
        self.thread.join(10)
        self.loop.close()
        for tool in self.tools:
            tool.close()
        self.provider.close()
        shutil.rmtree(self.root)

    def tool(self, style: str, port: int | None = None) -> FakeTool:
        tool = FakeTool(self.provider, style, port=port)
        self.tools.append(tool)
        return tool

    def launch(self, tool: FakeTool) -> subprocess.CompletedProcess[str]:
        result = tool.launch(self.root / "bin", {"STUDIO_BROWSER_SOCKET": str(self.root / "open.sock"),
                                                 "LAYR_SESSION": self.agent_id})
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def card(self) -> dict[str, Any]:
        self.bridge.poll(0)
        [card] = self.runtime.cards()
        return card

    def sign_in(self, tool: FakeTool) -> tuple[int, dict[str, str], bytes]:
        """Approve the card, then act as the browser: provider, then the loopback redirect."""
        card = self.card()
        self.assertEqual((card["method"], card["status"], card["agent"]), ("browser/open", "pending", self.agent_id))
        result = self.bridge.answer(card["id"], {"decision": "accept"}, local=True)
        self.assertEqual(result, {"status": "answered", "openUrl": card["params"]["url"]})
        location = provider_redirect(result["openUrl"])
        parts = urlsplit(location)
        self.assertEqual(parts.port, tool.port)
        [forward] = self.bridge.forwards.values()
        target = (parts.path or "/") + "?" + parts.query
        return browser_get(forward.bound_port, target, {"Cookie": "studio=mac-secret",
                                                        "Authorization": "Bearer mac", "User-Agent": "browser"})

    def assert_completed(self, tool: FakeTool, response: tuple[int, dict[str, str], bytes]) -> None:
        status, headers, body = response
        self.assertEqual(status, 200)
        self.assertIn(b"Signed in", body)
        self.assertEqual(headers["content-security-policy"], SANDBOX)
        self.assertEqual((headers["cache-control"], headers["x-content-type-options"]), ("no-store", "nosniff"))
        self.assertNotIn("set-cookie", headers)
        self.assertNotIn("location", headers)
        # The tool got the code once, without the Mac's cookies, and sent one token request.
        self.assertEqual(len(tool.codes), 1)
        self.assertEqual(len(self.provider.tokens), 1)
        self.assertEqual(self.provider.tokens[0]["code"], tool.codes)
        callback = tool.received[-1][1]
        self.assertNotIn("cookie", callback)
        self.assertNotIn("authorization", callback)
        self.assertEqual(callback["user-agent"], "browser")
        # The forward closed after the code, and the guest request closed with it.
        wait_until(lambda: self.bridge.forwards == {})
        [item] = self.guest_bridge.items.values()
        self.assertEqual((item["state"], item["closeReason"]), ("closed", "completed"))
        [card] = self.runtime.cards()
        self.assertEqual((card["status"], card["decision"]), ("answered", "accept"))

    def test_kubelogin_style_xdg_open_fixed_port_and_its_own_start_page(self) -> None:
        tool = self.tool("kubelogin", port=free_port(8000))
        result = self.launch(tool)
        self.assertIn("Studio asks the person to open 127.0.0.1", result.stderr)
        card = self.card()
        self.assertEqual(card["params"]["redirect"], {"host": "localhost", "port": tool.port, "path": "/"})
        self.assertEqual(card["params"]["host"], "127.0.0.1")
        self.assertTrue(card["params"]["url"].startswith(f"http://{self.provider.origin}/authorize?"))
        self.assert_completed(tool, self.sign_in(tool))

    def test_aws_style_browser_variable_random_port_and_provider_link(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card = self.card()
        self.assertEqual(card["params"]["url"], tool.open_url)
        self.assertEqual(card["params"]["redirect"], {"host": "127.0.0.1", "port": tool.port, "path": "/oauth/callback"})
        self.assertEqual((card["params"]["clientId"], card["params"]["scope"]), ("fake-client", "openid"))
        self.assert_completed(tool, self.sign_in(tool))

    def test_a_remote_window_cannot_open_and_nothing_binds(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card = self.card()
        with self.assertRaisesRegex(ValueError, "Open this chat in the window of the Mac"):
            self.bridge.answer(card["id"], {"decision": "accept"}, local=False)
        self.assertEqual(REMOTE_MESSAGE, "Open this chat in the window of the Mac that runs its server to sign in, "
                                         "or use the tool's device-code mode.")
        self.assertEqual(self.bridge.forwards, {})
        self.assertNotIn("browser.answer", self.guest.methods)
        self.assertEqual(self.runtime.cards()[0]["status"], "pending")

    def test_decline_tells_the_chat_and_closes_the_guest_request(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card = self.card()
        self.assertEqual(self.bridge.answer(card["id"], {"decision": "decline"}, local=False), {"status": "answered"})
        [event] = self.runtime.events
        self.assertEqual(event[:2], (self.agent_id, "browser_declined"))
        self.assertIn("declined to open 127.0.0.1", event[2])
        [item] = self.guest_bridge.items.values()
        self.assertEqual(item["state"], "declined")
        self.assertEqual(self.bridge.answer(card["id"], {"decision": "decline"}, local=True),
                         {"status": "answered", "replayed": True})
        with self.assertRaisesRegex(ValueError, "different answer"):
            self.bridge.answer(card["id"], {"decision": "accept"}, local=True)

    def test_a_stopped_tool_expires_the_card_and_frees_the_port(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card = self.card()
        self.tools.remove(tool)
        tool.close()
        with self.assertRaisesRegex(ValueError, "already stopped"):
            self.bridge.answer(card["id"], {"decision": "accept"}, local=True)
        self.assertEqual(self.runtime.cards()[0]["status"], "expired")
        self.assertEqual(self.bridge.forwards, {})

    def test_the_card_follows_the_guest_state_and_appears_once(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        self.card()
        self.bridge.after = 0
        self.bridge.poll(0)
        self.assertEqual(len(self.runtime.cards()), 1)
        self.launch(tool)  # The new link of the chat replaces the first one.
        self.bridge.poll(0)
        cards = sorted(self.runtime.cards(), key=lambda card: float(card["createdAt"]))
        self.assertEqual([card["status"] for card in cards], ["expired", "pending"])
        self.assertEqual(cards[0]["closeReason"], "replaced")


class Refusals(unittest.TestCase):
    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp())
        self.runtime = Runtime(self.root / "runtime.sqlite3")
        self.closed: list[str] = []
        owner = self

        class Guest:
            def call(self, method: str, params: dict[str, Any] | None = None, *,
                     request_id: str | None = None, timeout: float = 60) -> Any:
                owner.closed.append(method)
                return {"state": "closed"}
        self.bridge = Bridge(self.runtime, Guest)
        with self.runtime.db() as db:
            self.runtime.put(db, "agents", {"id": "native", "executionMode": "native"})
            self.runtime.put(db, "agents", {"id": "vm", "executionMode": "vm", "layrProjectId": "p1"})

    def tearDown(self) -> None:
        shutil.rmtree(self.root)

    def item(self, **fields: Any) -> dict[str, Any]:
        return {"id": uuid.uuid4().hex, "agentId": "vm", "projectId": "p1", "state": "pending",
                "url": "https://idp.example.com/a?redirect_uri=http%3A%2F%2F127.0.0.1%3A9000%2Fcb",
                "host": "idp.example.com", "redirect": {"host": "127.0.0.1", "port": 9000, "path": "/cb"},
                **fields}

    def test_links_that_fail_the_server_check_get_no_card(self) -> None:
        log = io.StringIO()
        with redirect_stderr(log):
            self.refuse_all()
        self.assertEqual(log.getvalue().count("browser bridge refused link"), 6)
        self.assertEqual(self.runtime.cards(), [])
        self.assertEqual(self.closed, ["browser.close"] * 6)
        good = self.item()
        self.bridge.apply(good)
        self.bridge.apply(good)
        self.assertEqual([card["id"] for card in self.runtime.cards()], [card_id(good["id"])])

    def refuse_all(self) -> None:
        for fields in ({"agentId": "native"}, {"agentId": "missing"}, {"projectId": "p2"},
                       {"url": "http://idp.example.com/a"}, {"redirect": None},
                       {"url": "https://idp.example.com/a?redirect_uri=http%3A%2F%2F127.0.0.1%3A9001%2Fcb"}):
            with self.subTest(fields=fields):
                self.bridge.apply(self.item(**fields))


class Relay(unittest.TestCase):
    def forward(self, relay: Any, path: str = "/cb") -> tuple[Forward, list[str]]:
        reasons: list[str] = []
        forward = Forward("b1", {"host": "127.0.0.1", "port": 9000, "path": path}, relay,
                          lambda _forward, reason: reasons.append(reason), listen_port=0)
        forward.bind()
        forward.start()
        self.addCleanup(forward.close, "test")
        return forward, reasons

    def test_only_the_redirect_get_reaches_the_tool_and_the_reply_is_rewritten(self) -> None:
        calls: list[tuple[str, dict[str, str]]] = []

        def relay(target: str, headers: dict[str, str]) -> dict[str, Any]:
            calls.append((target, headers))
            body = base64.b64encode(b"<script>steal()</script>ok").decode()
            return {"status": 200, "contentType": "application/javascript", "body": body, "done": False}
        forward, reasons = self.forward(relay)
        port = forward.bound_port
        self.assertEqual(browser_get(port, "/cb?code=1", method="POST")[0], 405)
        self.assertEqual(browser_get(port, "/other?code=1")[0], 404)
        self.assertEqual(browser_get(port, "/cb/../x")[0], 404)
        status, headers, body = browser_get(port, "/cb?state=1", {"Cookie": "a=b", "Origin": "http://x",
                                                                   "Referer": "https://idp", "Accept": "text/html"})
        self.assertEqual((status, headers["content-type"], body), (200, "text/plain; charset=utf-8",
                                                                  b"<script>steal()</script>ok"))
        self.assertEqual(headers["content-security-policy"], SANDBOX)
        self.assertEqual(calls, [("/cb?state=1", {"accept": "text/html"})])
        for _ in range(4):
            browser_get(port, "/cb")
        wait_until(lambda: reasons == ["limit"])
        with self.assertRaises(OSError):
            browser_get(port, "/cb")

    def test_an_unknown_relay_result_is_never_sent_again(self) -> None:
        def relay(target: str, headers: dict[str, str]) -> dict[str, Any]:
            raise LinuxVMError("lost", uncertain=True)
        forward, reasons = self.forward(relay)
        status, _, body = browser_get(forward.bound_port, "/cb?code=1")
        self.assertEqual(status, 502)
        self.assertIn(b"unknown", body)
        wait_until(lambda: reasons == ["unknown"])

    def test_cleaned_results(self) -> None:
        self.assertEqual(cleaned({"status": 302, "body": ""})[0], 200)
        self.assertEqual(cleaned({"status": 400, "contentType": "text/html; charset=ISO-8859-1",
                                  "body": base64.b64encode(b"x").decode()}),
                         (400, "text/html; charset=iso-8859-1", b"x"))
        invalid: list[Any] = [{"status": 99}, {"status": "200"}, {"status": 200, "body": "%%"},
                              {"status": 200, "body": base64.b64encode(b"x" * (256 * 1024 + 1)).decode()}, []]
        for result in invalid:
            with self.subTest(result=str(result)[:40]):
                with self.assertRaises(ValueError):
                    cleaned(result)


class Binds(unittest.TestCase):
    """A forward holds the port on every address the browser can use, or binds nothing."""

    def holder(self, family: socket.AddressFamily, address: str, port: int = 0, v6only: int | None = None) -> int:
        holder = socket.socket(family)
        self.addCleanup(holder.close)
        if v6only is not None:
            holder.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, v6only)
        holder.bind((address, port))
        holder.listen()
        return int(holder.getsockname()[1])

    def attempt(self, host: str, port: int) -> Forward:
        forward = Forward("b", {"host": host, "port": port, "path": "/"}, lambda target, headers: None,
                          lambda _forward, _reason: None)
        forward.bind()
        self.addCleanup(forward.close, "test")
        return forward

    def test_a_port_that_another_program_can_receive_on_is_refused(self) -> None:
        cases = [("127.0.0.1", socket.AF_INET, "0.0.0.0", None), ("127.0.0.1", socket.AF_INET6, "::", 0),
                 ("localhost", socket.AF_INET, "127.0.0.1", None), ("localhost", socket.AF_INET6, "::1", 1)]
        for host, family, address, v6only in cases:
            with self.subTest(host=host, held=address):
                port = self.holder(family, address, v6only=v6only)
                with self.assertRaisesRegex(PortBusy, f"Port {port} is in use on this Mac"):
                    self.attempt(host, port)
                if address == "::1":
                    # The bind of 127.0.0.1 that came first does not stay.
                    with socket.socket() as probe:
                        probe.bind(("127.0.0.1", port))

    def test_localhost_binds_both_loopback_addresses(self) -> None:
        port = free_port()
        forward = self.attempt("localhost", port)
        self.assertEqual(sorted(sock.family for sock in forward.sockets), sorted({socket.AF_INET, socket.AF_INET6}))
        self.assertEqual(forward.bound_port, port)


class Wiring(unittest.TestCase):
    def test_runtime_answer_sends_sign_in_cards_to_the_bridge_with_the_window_flag(self) -> None:
        from codex_runtime import Runtime as Real
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        runtime = Runtime(root / "runtime.sqlite3")
        with runtime.db() as db:
            runtime.put(db, "requests", {"id": "card", "method": "browser/open", "status": "pending", "params": {}})
        calls: list[tuple[str, bool]] = []

        def answer(owner: Any, key: str, data: dict[str, Any], *, local: bool) -> dict[str, Any]:
            calls.append((key, local))
            return {"status": "answered"}
        with patch.object(bridge_module, "answer", answer):
            self.assertEqual(Real.answer(runtime, "card", {"decision": "accept"}, local_window=True),  # type: ignore[arg-type]
                             {"status": "answered"})
            Real.answer(runtime, "card", {"decision": "accept"})  # type: ignore[arg-type]
        self.assertEqual(calls, [("card", True), ("card", False)])

    def test_only_a_direct_local_request_is_a_local_window(self) -> None:
        from starlette.requests import Request
        from studio_api.work.router import local_window

        def request(client: str | None, headers: dict[str, str] | None = None, **scope: Any) -> Request:
            return Request({"type": "http", "method": "POST", "path": "/api/answer", "query_string": b"",
                            "client": (client, 50000) if client else None,
                            "headers": [(name.encode(), value.encode()) for name, value in (headers or {}).items()],
                            **scope})
        self.assertTrue(local_window(request("127.0.0.1", {"host": "127.0.0.1:4620"})))
        self.assertTrue(local_window(request("::1")))
        self.assertTrue(local_window(request(None, extensions={"studio.unix_socket": True})))
        self.assertFalse(local_window(request("127.0.0.1", {"x-forwarded-for": "100.64.0.2"})))
        self.assertFalse(local_window(request("127.0.0.1", studio_principal={"kind": "device"})))
        self.assertFalse(local_window(request("100.64.0.2")))

    def test_the_vm_payload_installs_the_shim_and_the_socket_folder(self) -> None:
        from codex_linux_vm import _cloud_config
        config = _cloud_config(VM_GUEST_ROOT, "0.160.1", "2.1.291")
        files = {entry["path"]: gzip.decompress(base64.b64decode(entry["content"]))
                 for entry in config["write_files"] if entry["path"].startswith("/opt/codex-studio/vm/guest/")}
        for name in ("browser_bridge.py", "browser_policy.py", "studio_browser.py"):
            self.assertEqual(files["/opt/codex-studio/vm/guest/" + name], (VM_GUEST_ROOT / name).read_bytes())
        self.assertFalse(any("test_browser" in path for path in files))
        install = files["/opt/codex-studio/vm/guest/install.sh"].decode().splitlines()
        self.assertIn('install -m 0755 -o root -g root "$script_dir/studio_browser.py" /usr/local/bin/studio-browser',
                      install)
        self.assertIn("ln -sfn /usr/local/bin/studio-browser /usr/local/bin/xdg-open", install)
        unit = files["/opt/codex-studio/vm/guest/codex-studio-guest.service"].decode().splitlines()
        self.assertIn("RuntimeDirectory=codex-studio-browser", unit)
        self.assertIn("RuntimeDirectoryMode=0755", unit)


if __name__ == "__main__":
    unittest.main()
