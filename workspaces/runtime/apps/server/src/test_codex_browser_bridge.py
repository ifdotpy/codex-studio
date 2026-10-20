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
import errno
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


class FlowBase(unittest.TestCase):
    """The real guest bridge, the real shim, a fake provider and fake tools (see the module text)."""

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


class Flow(FlowBase):
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



class PhoneWindow(FlowBase):
    """Paste mode (docs/vm-browser-bridge.md, section 6): a phone opens the link in its own
    browser; the redirect to the phone's loopback fails, and the person pastes its address."""

    def open_on_phone(self, tool: FakeTool) -> tuple[dict[str, Any], str]:
        """Approve in paste mode from a remote window, then act as the phone browser: the
        provider sends it to the loopback redirect, which does not load there."""
        card = self.card()
        result = self.bridge.answer(card["id"], {"decision": "accept", "paste": True}, local=False)
        self.assertEqual(result, {"status": "answered", "openUrl": card["params"]["url"]})
        # Nothing binds a port on this Mac; the card waits for the pasted address.
        self.assertEqual(self.bridge.forwards, {})
        [waiting] = self.runtime.cards()
        self.assertEqual((waiting["status"], waiting["params"]["phase"]), ("pending", "waiting"))
        self.assertGreater(waiting["params"]["waiting"]["expiresAt"], time.time())
        self.assertEqual(self.bridge.answer(card["id"], {"decision": "accept", "paste": True}, local=False),
                         {"status": "answered", "replayed": True, "openUrl": card["params"]["url"]})
        return card, provider_redirect(result["openUrl"])

    def paste(self, card: dict[str, Any], address: str) -> dict[str, Any]:
        result: dict[str, Any] = self.bridge.answer(card["id"], {"decision": "answer", "result": address},
                                                    local=False)
        return result

    def assert_signed_in(self, tool: FakeTool, card: dict[str, Any]) -> None:
        self.assertEqual(len(tool.codes), 1)
        self.assertEqual(len(self.provider.tokens), 1)
        [done] = self.runtime.cards()
        self.assertEqual((done["status"], done["params"]["outcome"], done["params"]["outcomeHttp"]),
                         ("answered", "signed-in", 200))
        self.assertNotIn("code=", json.dumps(done))  # Studio stores no pasted address.
        [event] = self.runtime.events
        self.assertEqual((event[0], event[1], event[3]),
                         (self.agent_id, "browser_outcome", "browser-outcome:" + card["params"]["bridgeId"]))
        self.assertIn("The person signed in at 127.0.0.1", event[2])
        [item] = self.guest_bridge.items.values()
        self.assertEqual((item["state"], item["closeReason"]), ("closed", "completed"))

    def test_phone_paste_relays_a_kubelogin_style_redirect_once(self) -> None:
        tool = self.tool("kubelogin", port=free_port(8000))
        self.launch(tool)
        card, address = self.open_on_phone(tool)
        self.assertEqual(urlsplit(address).netloc, f"localhost:{tool.port}")
        # Some phone browsers copy the address without the scheme.
        self.assertEqual(self.paste(card, address.removeprefix("http://")), {"status": "answered"})
        self.assert_signed_in(tool, card)
        # The tool's start page served the first hop; then only the one callback reached it.
        self.assertEqual([path.split("?")[0] for path, _ in tool.received], ["/", "/"])
        self.assertEqual(self.paste(card, address), {"status": "answered", "replayed": True})
        self.assertEqual(len(tool.codes), 1)

    def test_phone_paste_relays_an_aws_style_redirect_and_checks_the_state(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card, address = self.open_on_phone(tool)
        parts = urlsplit(address)
        refusals = {
            address.replace("state=", "state=x"): "another sign-in",
            address.replace(f":{tool.port}/", f":{free_port()}/"): "must use port",
            address.replace("/oauth/callback", "/other"): "must use port",
            f"http://127.0.0.1:{tool.port}/oauth/callback?state={tool.state}": "no sign-in result",
            "https://" + parts.netloc + parts.path + "?" + parts.query: "page that did not load",
            "http://evil.example:" + str(tool.port) + parts.path + "?" + parts.query: "page that did not load",
            "": "Give one link",
        }
        for pasted, message in refusals.items():
            with self.subTest(pasted=pasted), self.assertRaisesRegex(ValueError, message):
                self.paste(card, pasted)
        # Nothing reached the tool, and the card still waits without a paste mark.
        self.assertEqual(tool.received, [])
        [waiting] = self.runtime.cards()
        self.assertNotIn("paste", waiting["params"])
        self.assertEqual(self.paste(card, address), {"status": "answered"})
        self.assert_signed_in(tool, card)

    def test_a_link_without_state_cannot_take_a_pasted_address(self) -> None:
        tool = self.tool("aws")
        tool.open_url = tool.open_url.replace("&state=" + tool.state, "")
        self.launch(tool)
        card, address = self.open_on_phone(tool)
        with self.assertRaisesRegex(ValueError, "no state value"):
            self.paste(card, address)
        self.assertEqual(tool.received, [])

    def test_a_paste_after_the_lease_deadline_is_refused_and_expires_the_card(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card, address = self.open_on_phone(tool)
        self.guest_bridge.clock = lambda: time.time() + 3600
        with self.assertRaisesRegex(ValueError, "stopped waiting"):
            self.paste(card, address)
        self.assertEqual(tool.codes, [])
        [expired] = self.runtime.cards()
        self.assertEqual((expired["status"], expired["params"]["outcome"]), ("expired", "expired"))
        with self.assertRaisesRegex(ValueError, "does not wait"):
            self.paste(card, address)

    def test_a_lost_paste_reply_is_unknown_and_never_sent_again(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card, address = self.open_on_phone(tool)
        call = self.guest.call

        def lose_callback(method: str, params: dict[str, Any] | None = None, **options: Any) -> Any:
            result = call(method, params, **options)
            if method == "browser.callback":
                raise LinuxVMError("The guest reply was lost", uncertain=True, code="unavailable")
            return result
        with patch.object(self.guest, "call", side_effect=lose_callback):
            with self.assertRaisesRegex(ValueError, "unknown"):
                self.paste(card, address)
            self.assertEqual(self.paste(card, address), {"status": "answered", "replayed": True})
        self.assertEqual(self.guest.methods.count("browser.callback"), 1)
        self.assertEqual(len(tool.codes), 1)
        [card_after] = self.runtime.cards()
        self.assertEqual((card_after["status"], card_after["params"]["outcome"]), ("uncertain", "unknown"))

    def test_two_pastes_at_once_send_one_code(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card, address = self.open_on_phone(tool)
        release, entered = threading.Event(), threading.Event()
        call = self.guest.call

        def slow(method: str, params: dict[str, Any] | None = None, **options: Any) -> Any:
            if method == "browser.callback":
                entered.set()
                release.wait(10)
            return call(method, params, **options)
        results: list[object] = []
        with patch.object(self.guest, "call", side_effect=slow):
            first = threading.Thread(target=lambda: results.append(self.paste(card, address)))
            first.start()
            self.assertTrue(entered.wait(10))
            with self.assertRaisesRegex(ValueError, "already sends"):
                self.paste(card, address)
            release.set()
            first.join(10)
        self.assertEqual(results, [{"status": "answered"}])
        self.assertEqual(len(tool.codes), 1)

    def test_cancel_while_waiting_closes_the_guest_request(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card, address = self.open_on_phone(tool)
        self.assertEqual(self.bridge.answer(card["id"], {"decision": "decline"}, local=False), {"status": "answered"})
        [item] = self.guest_bridge.items.values()
        self.assertEqual((item["state"], item["closeReason"]), ("closed", "cancelled"))
        [done] = self.runtime.cards()
        self.assertEqual((done["status"], done["decision"], done["params"]["outcome"]),
                         ("answered", "decline", "cancelled"))
        [event] = self.runtime.events
        self.assertEqual(event[1], "browser_declined")
        with self.assertRaisesRegex(ValueError, "does not wait"):
            self.paste(card, address)
        self.assertEqual(tool.received, [])

    def test_the_guest_ending_a_waiting_sign_in_tells_the_chat(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card, _ = self.open_on_phone(tool)
        self.tools.remove(tool)
        tool.close()
        asyncio.run_coroutine_threadsafe(self.guest_bridge.check(), self.loop).result(10)
        self.bridge.poll(0)
        [done] = self.runtime.cards()
        self.assertEqual((done["status"], done["closeReason"], done["params"]["outcome"]),
                         ("expired", "stale", "expired"))
        [event] = self.runtime.events
        self.assertEqual(event[1], "browser_outcome")
        self.assertIn("ended without a result", event[2])

    def test_device_code_opens_from_a_phone_without_a_forward(self) -> None:
        tool = self.tool("device")
        self.launch(tool)
        card = self.card()
        self.assertEqual((card["params"]["redirect"], card["params"]["userCode"]), (None, "ABCD-EFGH"))
        # An older window without paste mode still gets the remote refusal.
        with self.assertRaisesRegex(ValueError, "Open this chat in the window of the Mac"):
            self.bridge.answer(card["id"], {"decision": "accept"}, local=False)
        result = self.bridge.answer(card["id"], {"decision": "accept", "paste": True}, local=False)
        self.assertEqual(result, {"status": "answered", "openUrl": tool.open_url})
        self.assertEqual(self.bridge.forwards, {})
        [item] = self.guest_bridge.items.values()
        self.assertEqual(item["state"], "opened")
        with urllib.request.urlopen(result["openUrl"], timeout=5) as response:
            self.assertEqual(response.status, 200)
        self.assertEqual(self.provider.devices, ["ABCD-EFGH"])
        self.assertEqual(self.bridge.answer(card["id"], {"decision": "accept", "paste": True}, local=False),
                         {"status": "answered", "replayed": True, "openUrl": tool.open_url})


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
        # The AWS CLI callback page has no Content-Type; it renders as HTML, other text stays plain.
        aws = b"\n  <!DOCTYPE html>\n<html lang=en><title>Amazon Web Services Sign-in</title>"
        self.assertEqual(cleaned({"status": 200, "body": base64.b64encode(aws).decode()}),
                         (200, "text/html; charset=utf-8", aws))
        self.assertEqual(cleaned({"status": 200, "body": base64.b64encode(b"done").decode()})[1],
                         "text/plain; charset=utf-8")
        self.assertEqual(cleaned({"status": 200, "contentType": "application/json",
                                  "body": base64.b64encode(b"<html>").decode()})[1],
                         "text/plain; charset=utf-8")
        invalid: list[Any] = [{"status": 99}, {"status": "200"}, {"status": 200, "body": "%%"},
                              {"status": 200, "body": base64.b64encode(b"x" * (256 * 1024 + 1)).decode()}, []]
        for result in invalid:
            with self.subTest(result=str(result)[:40]):
                with self.assertRaises(ValueError):
                    cleaned(result)


class Binds(unittest.TestCase):
    """A forward holds the port on every address the browser can use, or binds nothing."""

    def holder(self, family: socket.AddressFamily, address: str, port: int = 0,
               v6only: int | None = None) -> tuple[socket.socket, int]:
        holder = socket.socket(family)
        try:
            if v6only is not None:
                holder.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, v6only)
            holder.bind((address, port))
            holder.listen()
            return holder, int(holder.getsockname()[1])
        except BaseException:
            holder.close()
            raise

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
                holder, port = self.holder(family, address, v6only=v6only)
                try:
                    with self.assertRaisesRegex(PortBusy, f"Port {port} is in use on this Mac"):
                        self.attempt(host, port)
                finally:
                    holder.close()

    def test_localhost_ipv6_conflict_closes_partial_ipv4_reservation(self) -> None:
        class FakeSocket:
            def __init__(self, family: socket.AddressFamily) -> None:
                self.family = family
                self.closed = False

            def setsockopt(self, *_args: Any) -> None:
                pass

            def bind(self, address: tuple[str, int]) -> None:
                if address[0] == "::1":
                    raise OSError(errno.EADDRINUSE, "occupied IPv6 loopback")

            def listen(self, _backlog: int) -> None:
                pass

            def getsockname(self) -> tuple[str, int]:
                return "127.0.0.1", 43210

            def close(self) -> None:
                self.closed = True

        sockets: list[FakeSocket] = []

        def make_socket(family: socket.AddressFamily, _kind: int) -> FakeSocket:
            listener = FakeSocket(family)
            sockets.append(listener)
            return listener

        forward = Forward(
            "b",
            {"host": "localhost", "port": 43210, "path": "/"},
            lambda _target, _headers: None,
            lambda _forward, _reason: None,
        )
        with patch.object(socket, "socket", make_socket):
            with self.assertRaisesRegex(PortBusy, "Port 43210 is in use"):
                forward.bind()

        self.assertEqual(len(sockets), 2)
        self.assertTrue(all(listener.closed for listener in sockets))
        self.assertEqual(forward.sockets, [])

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



class RemoteWindow(Flow):
    """A window on another Mac: its own server holds the port and relays to this server."""

    def remote_sign_in(self, tool: FakeTool, bridge: Bridge, card: dict[str, Any],
                       token: str | None = None) -> tuple[int, dict[str, str], bytes]:
        result = bridge.answer(card["id"], {"decision": "accept", "forward": True}, local=False)
        forward = result["forward"]
        self.assertEqual(result["openUrl"], card["params"]["url"])
        self.assertEqual((forward["server"], forward["bridgeId"], forward["port"]),
                         ("srv-b", card["params"]["bridgeId"], tool.port))
        # Nothing binds on the chat's server; the window's server binds the port.
        self.assertEqual(bridge.forwards, {})
        self.assertNotIn(forward["token"], json.dumps(self.runtime.cards()))
        client = Runtime(self.root / "client.sqlite3")
        sent: list[tuple[str, str]] = []

        def exchange(server: str, action: str, payload: dict[str, Any]) -> Any:
            sent.append((server, action))
            return bridge_module.remote_callback(self.runtime, payload)
        data = {**forward, "token": token or forward["token"]}
        self.assertEqual(bridge_module.open_remote_forward(client, data, exchange, listen_port=0),
                         {"status": "forwarding"})
        [remote] = client.__dict__["_browser_remote_forwards"].values()
        location = provider_redirect(result["openUrl"])
        parts = urlsplit(location)
        response = browser_get(remote.bound_port, (parts.path or "/") + "?" + parts.query,
                               {"Cookie": "studio=mac-secret", "Authorization": "Bearer mac",
                                "User-Agent": "browser"})
        self.assertEqual(sent, [("srv-b", "browser_callback")])
        wait_until(lambda: client.__dict__["_browser_remote_forwards"] == {})
        return response

    def test_a_vm_chat_signs_in_from_a_window_on_another_mac(self) -> None:
        self.bridge.server_id = lambda: "srv-b"
        tool = self.tool("aws")
        self.launch(tool)
        card = self.card()
        with patch.object(bridge_module, "bridge", return_value=self.bridge):
            response = self.remote_sign_in(tool, self.bridge, card)
        status, headers, body = response
        self.assertEqual(status, 200)
        self.assertIn(b"Signed in", body)
        self.assertEqual(headers["content-security-policy"], SANDBOX)
        self.assertEqual(len(tool.codes), 1)
        self.assertEqual(len(self.provider.tokens), 1)
        self.assertNotIn("cookie", tool.received[-1][1])
        [card] = self.runtime.cards()
        self.assertEqual((card["status"], card["decision"]), ("answered", "accept"))

    def test_a_wrong_forward_token_never_reaches_the_tool(self) -> None:
        self.bridge.server_id = lambda: "srv-b"
        tool = self.tool("aws")
        self.launch(tool)
        card = self.card()
        with patch.object(bridge_module, "bridge", return_value=self.bridge):
            status, _headers, body = self.remote_sign_in(tool, self.bridge, card, token="x" * 43)
        self.assertEqual(status, 502)
        self.assertIn(b"unknown", body)
        self.assertEqual(tool.codes, [])
        with patch.object(bridge_module, "bridge", return_value=self.bridge):
            with self.assertRaisesRegex(PermissionError, "token"):
                bridge_module.remote_callback(self.runtime, {"bridgeId": card["params"]["bridgeId"],
                                                             "token": "y" * 43, "target": "/oauth/callback"})

    def test_an_older_window_still_gets_the_remote_message(self) -> None:
        tool = self.tool("aws")
        self.launch(tool)
        card = self.card()
        with self.assertRaisesRegex(ValueError, "Open this chat in the window of the Mac"):
            self.bridge.answer(card["id"], {"decision": "accept"}, local=False)
        self.assertNotIn("browser.answer", self.guest.methods)


class NativeChats(unittest.TestCase):
    """A native chat of this server: the launcher in its session, the host queue and card."""

    def setUp(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="sbh-", dir="/tmp"))
        self.provider = FakeProvider()
        self.tools: list[FakeTool] = []
        self.agent_id, self.thread = str(uuid.uuid4()), str(uuid.uuid4())
        self.runtime = Runtime(self.root / "runtime.sqlite3")
        self.runtime.root = self.root  # type: ignore[attr-defined]
        with self.runtime.db() as db:
            self.runtime.put(db, "agents", {"id": self.agent_id, "threadId": self.thread, "provider": "claude",
                                            "epoch": 0, "autoWake": True})
            self.runtime.put(db, "agents", {"id": str(uuid.uuid4()), "threadId": str(uuid.uuid4()),
                                            "executionMode": "vm"})
        from codex_browser_host import HostGuest
        self.guest = HostGuest(self.runtime, listeners=lambda: [Listener("127.0.0.1", tool.port, UID)
                                                                 for tool in self.tools],
                               http_providers=frozenset({self.provider.origin}))
        self.bridge = Bridge(self.runtime, lambda: self.guest, source="host", server_id=lambda: "srv-b",
                             http_providers=frozenset({self.provider.origin}))

    def tearDown(self) -> None:
        self.bridge.stop()
        self.guest.close()
        for tool in self.tools:
            tool.close()
        self.provider.close()
        shutil.rmtree(self.root)

    def launch(self, session: str) -> tuple[FakeTool, subprocess.CompletedProcess[str]]:
        from codex_browser_host import session_environment
        tool = FakeTool(self.provider, "aws")
        self.tools.append(tool)
        environment = session_environment(self.root)
        # The tool runs the launcher that the session environment names.
        result = tool.launch(Path(environment["BROWSER"]).parent,
                             {"STUDIO_BROWSER_SOCKET": environment["STUDIO_BROWSER_SOCKET"],
                              "STUDIO_BROWSER_SESSION": session})
        return tool, result

    def test_the_session_launcher_queues_a_card_for_the_chat_of_its_thread(self) -> None:
        tool, result = self.launch(self.thread)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.bridge.poll(0)
        [card] = self.runtime.cards()
        self.assertEqual((card["agent"], card["params"]["source"]), (self.agent_id, "host"))
        self.assertEqual(card["params"]["redirect"]["port"], tool.port)
        # A window on this Mac reaches the tool directly: nothing binds.
        answer = self.bridge.answer(card["id"], {"decision": "accept"}, local=True)
        self.assertEqual(answer, {"status": "answered", "openUrl": card["params"]["url"]})
        self.assertEqual(self.bridge.forwards, {})
        parts = urlsplit(provider_redirect(answer["openUrl"]))
        status, _headers, _body = browser_get(tool.port, (parts.path or "/") + "?" + parts.query)
        self.assertEqual(status, 200)
        self.assertEqual(len(tool.codes), 1)

    def test_a_phone_signs_in_to_a_command_of_a_headless_server_by_paste(self) -> None:
        tool, result = self.launch(self.thread)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.bridge.poll(0)
        [card] = self.runtime.cards()
        answer = self.bridge.answer(card["id"], {"decision": "accept", "paste": True}, local=False)
        self.assertEqual(answer, {"status": "answered", "openUrl": card["params"]["url"]})
        # The phone's browser lands on its own loopback; the person pastes that address.
        address = provider_redirect(answer["openUrl"])
        self.assertEqual(self.bridge.answer(card["id"], {"decision": "answer", "result": address}, local=False),
                         {"status": "answered"})
        self.assertEqual(len(tool.codes), 1)
        [done] = self.runtime.cards()
        self.assertEqual((done["status"], done["params"]["outcome"]), ("answered", "signed-in"))

    def test_private_control_socket_answers_a_host_browser_request(self) -> None:
        tool, result = self.launch(self.agent_id)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.bridge.poll(0)
        [card] = self.runtime.cards()
        control = self.root / "browser-host" / "control.sock"
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(5)
            client.connect(str(control))
            client.sendall(json.dumps({
                "id": "host-browser-answer-test",
                "method": "browser.answer",
                "params": {"id": card["params"]["bridgeId"], "decision": "approve"},
            }).encode() + b"\n")
            with client.makefile("rb") as response:
                answer = json.loads(response.readline())
        self.assertEqual(answer["id"], "host-browser-answer-test")
        self.assertEqual(answer["result"]["state"], "approved")
        parts = urlsplit(provider_redirect(card["params"]["url"]))
        status, _headers, _body = browser_get(tool.port, (parts.path or "/") + "?" + parts.query)
        self.assertEqual(status, 200)
        self.assertEqual(len(tool.codes), 1)

    def test_close_cancels_a_pending_control_socket_long_poll(self) -> None:
        errors: list[Exception] = []

        def wait_for_browser() -> None:
            try:
                self.guest.call("browser.wait", {"after": 0, "waitSeconds": 20}, timeout=35)
            except Exception as error:
                errors.append(error)

        client = threading.Thread(target=wait_for_browser, daemon=True)
        client.start()
        wait_until(lambda: bool(self.guest.control_tasks))
        started = time.monotonic()
        self.guest.close()
        self.assertLess(time.monotonic() - started, 2)
        client.join(timeout=2)
        self.assertFalse(client.is_alive(), "closing the host bridge must release its control RPC")
        self.assertTrue(errors)

    def test_close_refuses_a_control_call_whose_handler_starts_after_shutdown_began(self) -> None:
        # asyncio accepts a connection and starts its handler task in later loop steps, so
        # close() can snapshot control_tasks before the handler registers. Hold the handler
        # until the control server is closed: that is the same step as the snapshot.
        from codex_browser_host import HostGuest
        original = HostGuest.control_client
        entered = threading.Event()
        release: list[asyncio.Event] = []

        async def held_client(guest: Any, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
            release.append(asyncio.Event())
            entered.set()
            await release[0].wait()
            await original(guest, reader, writer)

        self.guest.close()
        with patch.object(HostGuest, "control_client", held_client):
            self.guest = HostGuest(self.runtime, listeners=lambda: [])
        server = self.guest.control_server
        close_server = server.close

        def close_then_release() -> None:
            close_server()
            release[0].set()
        server.close = close_then_release  # type: ignore[method-assign]
        errors: list[Exception] = []

        def wait_for_browser() -> None:
            try:
                self.guest.call("browser.wait", {"after": 0, "waitSeconds": 20}, timeout=35)
            except Exception as error:
                errors.append(error)

        client = threading.Thread(target=wait_for_browser, daemon=True)
        client.start()
        self.assertTrue(entered.wait(5))
        started = time.monotonic()
        self.guest.close()
        self.assertLess(time.monotonic() - started, 2)
        client.join(timeout=2)
        self.assertFalse(client.is_alive(), "closing the host bridge must refuse a late control RPC")
        self.assertTrue(errors)

    def test_a_window_on_another_mac_relays_through_its_own_server(self) -> None:
        tool, result = self.launch(self.agent_id)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.bridge.poll(0)
        [card] = self.runtime.cards()
        answer = self.bridge.answer(card["id"], {"decision": "accept", "forward": True}, local=False)
        forward = answer["forward"]
        client = Runtime(self.root / "client.sqlite3")
        with patch.object(bridge_module, "host_bridge", return_value=self.bridge):
            bridge_module.open_remote_forward(
                client, forward, lambda server, action, payload: bridge_module.remote_callback(self.runtime, payload),
                listen_port=0)
            [remote] = client.__dict__["_browser_remote_forwards"].values()
            parts = urlsplit(provider_redirect(answer["openUrl"]))
            status, headers, body = browser_get(remote.bound_port, (parts.path or "/") + "?" + parts.query,
                                                {"Cookie": "studio=mac-secret", "User-Agent": "browser"})
        self.assertEqual(status, 200)
        self.assertIn(b"Signed in", body)
        self.assertEqual(headers["content-security-policy"], SANDBOX)
        self.assertEqual(len(tool.codes), 1)
        self.assertNotIn("cookie", tool.received[-1][1])
        # A second approval reply is a replay with the same forward.
        replay = self.bridge.answer(card["id"], {"decision": "accept", "forward": True}, local=False)
        self.assertEqual(replay["forward"], forward)

    def test_unknown_and_vm_sessions_get_no_card(self) -> None:
        with self.runtime.db() as db:
            vm_thread = [json.loads(row[0])["threadId"] for row in db.execute("SELECT record FROM runtime_agents")
                         if json.loads(row[0]).get("executionMode") == "vm"][0]
        for session in (str(uuid.uuid4()), vm_thread):
            _tool, result = self.launch(session)
            self.assertEqual(result.returncode, 1)
            self.assertIn("does not know the chat", result.stderr)
        self.bridge.poll(0)
        self.assertEqual(self.runtime.cards(), [])

    def test_the_bridge_file_names_the_launcher_and_the_socket(self) -> None:
        from codex_browser_host import (
            FOLDER,
            HOST_BROWSER_CONTROL_SOCKET_NAME,
            SESSION_FILE,
            publish,
        )
        account = self.root / "account-servers" / "claude-local"
        account.mkdir(parents=True)
        publish(account, self.root)
        value = json.loads((account / SESSION_FILE).read_text())
        self.assertEqual(value, {"BROWSER": str(self.root / "browser-host" / "studio-browser"),
                                 "STUDIO_BROWSER_SOCKET": str(self.root / "browser-host" / "open.sock"),
                                 "STUDIO_BROWSER_PATH": str(self.root / "browser-host" / "bin")})
        self.assertEqual((account / SESSION_FILE).stat().st_mode & 0o777, 0o600)
        self.assertEqual((self.root / "browser-host").stat().st_mode & 0o777, 0o700)
        repository = Path(__file__).resolve().parents[5]
        rust_router = repository / "workspaces/runtime/packages/runtime-http/src/router.rs"
        rust_routes = repository / "workspaces/runtime/packages/runtime-http/src/routes/questions_write.rs"
        services = repository / "workspaces/runtime/packages/runtime-services/src"
        router_source = rust_router.read_text(encoding="utf-8")
        route_source = rust_routes.read_text(encoding="utf-8")
        environment_source = (services / "browser_environment.rs").read_text(encoding="utf-8")
        host_source = (services / "browser_host.rs").read_text(encoding="utf-8")
        # The Rust owners of the names, and the router that consumes them.
        self.assertIn(f'pub const BROWSER_HOST_FOLDER: &str = "{FOLDER}"', environment_source)
        self.assertIn(f'pub const CONTROL_SOCKET: &str = "{HOST_BROWSER_CONTROL_SOCKET_NAME}"', host_source)
        self.assertIn(f'pub const SESSION_FILE: &str = "{SESSION_FILE}"', host_source)
        self.assertIn("studio_runtime_services::browser_environment::BROWSER_HOST_FOLDER", router_source)
        self.assertIn("studio_runtime_services::browser_host::CONTROL_SOCKET", router_source)
        self.assertIn(".join(BROWSER_HOST_FOLDER_NAME)", router_source)
        self.assertIn(".join(crate::router::BROWSER_HOST_FOLDER_NAME)", route_source)
        self.assertIn(
            ".join(crate::router::BROWSER_HOST_CONTROL_SOCKET_NAME)", route_source
        )


class HostParts(unittest.TestCase):
    def test_lsof_listeners_give_addresses_ports_and_owners(self) -> None:
        from codex_browser_host import lsof_listeners
        text = "p10\nu501\nf3\ntIPv4\nn127.0.0.1:8000\nf4\ntIPv6\nn*:7000\np11\nu0\nf5\ntIPv4\nn*:9000\nf6\ntIPv6\nn[::1]:9100\n"
        self.assertEqual(lsof_listeners(text), [Listener("127.0.0.1", 8000, 501), Listener("::", 7000, 501),
                                                Listener("0.0.0.0", 9000, 0), Listener("::1", 9100, 0)])

    def test_the_paired_exchange_sends_each_redirect_once_and_keeps_unknown_unknown(self) -> None:
        class Service:
            def __init__(self, outcome: dict[str, Any]) -> None:
                self.outcome = outcome
                self.queued: list[tuple[str, str, str]] = []
                self.transport = type("T", (), {"servers": staticmethod(lambda: [{"id": "srv-b"}])})()

            def queue(self, db: Any, server: str, action: str, payload: dict[str, Any], key: str) -> str:
                self.queued.append((server, action, key))
                return key

            def deliver(self, key: str) -> dict[str, Any]:
                return self.outcome

        class Owner(Runtime):
            service: Any

            def multi_server(self) -> Any:
                return self.service
        runtime = Owner(Path(tempfile.mkdtemp(prefix="sbx-", dir="/tmp")) / "r.sqlite3")
        runtime.service = Service({"outcome": "applied", "value": {"status": 200}})
        exchange = bridge_module.paired_exchange(runtime)
        self.assertEqual(exchange("srv-b", "browser_callback", {}), {"status": 200})
        exchange("srv-b", "browser_callback", {})
        keys = [key for _s, _a, key in runtime.service.queued]
        self.assertEqual(len(set(keys)), 2)
        with self.assertRaisesRegex(ValueError, "not paired"):
            exchange("srv-c", "browser_callback", {})
        runtime.service.outcome = {"outcome": "not_applied", "error": "The sign-in forward is closed"}
        with self.assertRaisesRegex(ValueError, "closed"):
            exchange("srv-b", "browser_callback", {})
        runtime.service.outcome = {"outcome": "unknown"}
        with self.assertRaisesRegex(RuntimeError, "unknown"):
            exchange("srv-b", "browser_callback", {})
        shutil.rmtree(runtime.path.parent)

    def test_a_client_forward_refuses_bad_shapes_and_a_closed_request(self) -> None:
        runtime = Runtime(Path(tempfile.mkdtemp(prefix="sbx-", dir="/tmp")) / "r.sqlite3")
        good = {"server": "srv-b", "bridgeId": "a" * 32, "host": "127.0.0.1", "port": free_port(),
                "path": "/oauth/callback", "expiresAt": time.time() + 60, "token": "t" * 43}
        for change in ({"host": "example.com"}, {"port": 80}, {"path": "x"}, {"bridgeId": "../x"}, {"token": ""}):
            with self.assertRaisesRegex(ValueError, "invalid"):
                bridge_module.open_remote_forward(runtime, {**good, **change}, lambda *a: None)
        with self.assertRaisesRegex(ValueError, "no longer open"):
            bridge_module.open_remote_forward(runtime, {**good, "expiresAt": time.time() - 1}, lambda *a: None)
        self.assertEqual(runtime.__dict__.get("_browser_remote_forwards", {}), {})
        shutil.rmtree(runtime.path.parent)

    def test_the_paired_channel_sends_browser_callback_to_the_bridge(self) -> None:
        from codex_multi_server_orchestration import MultiServerService
        owner = type("Owner", (), {"runtime": object()})()
        with patch.object(bridge_module, "remote_callback", return_value={"status": 200}) as relayed:
            value = MultiServerService._receive(owner, "srv-a", "browser_callback", {"bridgeId": "b"}, "k")
        self.assertEqual(value, {"status": 200})
        relayed.assert_called_once_with(owner.runtime, {"bridgeId": "b"})


if __name__ == "__main__":
    unittest.main()
