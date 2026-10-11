"""The guest side of the VM browser bridge: URL rules, peer identity, queue, lease and relay.

Run: python3 -B -m unittest discover -s workspaces/runtime/apps/vm-guest -p test_browser_bridge.py -v
The tests use a real Unix socket, the real shim as a subprocess and fake sign-in tools on the
loopback. A fake /proc/net/tcp table names the listener owners, so they also run on macOS.
"""
from __future__ import annotations

import asyncio
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))

from browser_bridge import (BridgeError, BrowserBridge, Listener, connect_address,  # noqa: E402
                            parse_listeners)
from browser_policy import PolicyError, Provider, StartPage, classify, provider  # noqa: E402
from test_browser_fakes import SHIM, FakeProvider, FakeTool, install_shim  # noqa: E402

AGENT = str(uuid.uuid4())
OTHER = str(uuid.uuid4())
UID = os.getuid()


class Policy(unittest.TestCase):
    def test_provider_links_and_their_loopback_redirect(self) -> None:
        checked = provider("https://accounts.example.com/oauth/authorize?client_id=kube&scope=openid"
                           "&redirect_uri=http%3A%2F%2Flocalhost%3A8000&state=s")
        self.assertEqual((checked.host, checked.client_id, checked.scope), ("accounts.example.com", "kube", "openid"))
        assert checked.redirect is not None
        self.assertEqual(checked.redirect.record(), {"host": "localhost", "port": 8000, "path": "/"})
        aws = provider("https://oidc.eu-central-1.amazonaws.com/authorize?redirect_uri="
                       "http%3A%2F%2F127.0.0.1%3A40645%2Foauth%2Fcallback&scopes=sso%3Aaccount%3Aaccess")
        assert aws.redirect is not None
        self.assertEqual((aws.redirect.port, aws.redirect.path, aws.scope), (40645, "/oauth/callback", "sso:account:access"))
        device = provider("https://device.sso.eu-central-1.amazonaws.com/?user_code=ABCD-EFGH")
        self.assertIsNone(device.redirect)
        remote = provider("https://idp.example.com/a?redirect_uri=https%3A%2F%2Fapp.example.com%2Fcb")
        self.assertIsNone(remote.redirect)

    def test_a_tool_page_on_the_loopback_is_a_start_page(self) -> None:
        self.assertEqual(classify("http://localhost:8000/"), StartPage("http://localhost:8000/", 8000))
        self.assertIsInstance(classify("http://127.0.0.1:18000/x"), StartPage)
        self.assertIsInstance(classify("http://[::1]:8000/"), StartPage)
        # A test provider on the loopback is a provider only when the guest flag names it.
        self.assertIsInstance(classify("http://127.0.0.1:5555/authorize", http_providers=frozenset({"127.0.0.1:5555"})),
                              Provider)

    def test_refused_links(self) -> None:
        refused = [
            "file:///etc/passwd", "javascript:alert(1)", "data:text/html,x", "vscode://x", "ftp://example.com/",
            "x-apple.systempreferences:", "https://user:pw@example.com/", "http://example.com/login",
            "https://127.0.0.1:4620/api/session", "https://localhost/", "https://10.0.0.1/", "https://192.168.1.1/",
            "https://[::1]/", "https://169.254.169.254/", "https://studio.localhost/",
            "http://localhost/", "http://localhost:80/", "http://localhost:1023/",
            "https://idp.example.com/a?redirect_uri=http%3A%2F%2Flocalhost",
            "https://idp.example.com/a?redirect_uri=http%3A%2F%2Flocalhost%3A80%2Fcb",
            "https://idp.example.com/a?redirect_uri=https%3A%2F%2Flocalhost%3A8000",
            "https://idp.example.com/a?redirect_uri=http%3A%2F%2Fu%40localhost%3A8000",
            "https://example.com/" + "a" * 8192, "https://example.com/a b", "https://example.com/\x00",
            "", 7, None,
        ]
        for url in refused:
            with self.subTest(url=str(url)[:60]):
                with self.assertRaises(PolicyError):
                    classify(url)
        self.assertEqual(len("https://example.com/" + "a" * (8192 - 20)), 8192)
        classify("https://example.com/" + "a" * (8192 - 20))


class AgentEnvironment(unittest.TestCase):
    def test_every_agent_process_gets_the_shim_as_browser_and_no_secret_replaces_it(self) -> None:
        from layr_agents import LAUNCH_ENV_NAMES, AgentHandlers
        handlers = AgentHandlers.__new__(AgentHandlers)
        handlers.layr_root = Path("/var/lib/codex-studio/layr")
        environment = handlers.agent_env({"home": "/home/a", "owner": "a", "agentId": AGENT})
        self.assertEqual(environment["BROWSER"], "/usr/local/bin/studio-browser")
        self.assertIn("BROWSER", LAUNCH_ENV_NAMES)


class Listeners(unittest.TestCase):
    TCP = ("  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
           "   0: 0100007F:1F40 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1010        0 1 1\n"
           "   1: 00000000:943B 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1011        0 2 1\n"
           "   2: 0100007F:1F41 0100007F:9000 01 00000000:00000000 00:00000000 00000000  1012        0 3 1\n")
    TCP6 = ("  sl  local_address                         remote_address                        st tx rx tr tm re uid\n"
            "   0: 00000000000000000000000001000000:1F40 00000000000000000000000000000000:0000 0A 0:0 0:0 0 1013 0 4\n")

    def test_proc_rows_give_address_port_and_owner(self) -> None:
        rows = parse_listeners(self.TCP) + parse_listeners(self.TCP6)
        self.assertEqual(rows, [Listener("127.0.0.1", 8000, 1010), Listener("0.0.0.0", 37947, 1011),
                                Listener("::1", 8000, 1013)])

    def test_every_reachable_listener_must_belong_to_the_requester(self) -> None:
        own = [Listener("127.0.0.1", 8000, 1010)]
        self.assertEqual(connect_address(1010, 8000, own), "127.0.0.1")
        self.assertEqual(connect_address(1011, 37947, [Listener("0.0.0.0", 37947, 1011)]), "127.0.0.1")
        self.assertEqual(connect_address(1013, 8000, [Listener("::1", 8000, 1013)]), "::1")
        with self.assertRaises(BridgeError) as other:
            connect_address(1010, 8000, own + [Listener("::1", 8000, 1013)])
        self.assertEqual(other.exception.code, "forbidden")
        with self.assertRaises(BridgeError) as missing:
            connect_address(1010, 9000, own)
        self.assertEqual(missing.exception.code, "stale")
        with self.assertRaises(BridgeError):
            connect_address(1011, 8000, own)


    @unittest.skipUnless(Path("/proc/net/tcp").exists(), "Linux /proc/net/tcp is required")
    def test_the_real_proc_table_names_this_process_as_owner(self) -> None:
        import socket
        from browser_bridge import proc_listeners
        with socket.socket() as listener, socket.socket(socket.AF_INET6) as six:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            six.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            six.bind(("::1", 0))
            six.listen()
            rows = proc_listeners()
            port, port6 = listener.getsockname()[1], six.getsockname()[1]
            self.assertIn(Listener("127.0.0.1", port, UID), rows)
            self.assertIn(Listener("::1", port6, UID), rows)
            self.assertEqual(connect_address(UID, port6, rows), "::1")


class Bridge(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        # A short folder: a macOS Unix socket path has at most 103 bytes.
        self.root = Path(tempfile.mkdtemp(prefix="sb-", dir="/tmp"))
        self.provider = FakeProvider()
        self.tools: list[FakeTool] = []
        self.owners: dict[int, int] = {}
        self.now = 1000.0
        self.contexts = {AGENT: {"uid": UID, "projectId": "p1", "owner": "studio-a-test"},
                         OTHER: {"uid": UID + 1, "projectId": "p1", "owner": "studio-a-other"}}

        async def context(agent: str) -> dict[str, object]:
            if agent not in self.contexts:
                raise LookupError(agent)
            return self.contexts[agent]

        def listeners() -> list[Listener]:
            return [Listener("127.0.0.1", tool.port, self.owners.get(tool.port, UID)) for tool in self.tools]

        self.bridge = BrowserBridge(self.root / "state", context, listeners=listeners,
                                    http_providers=frozenset({self.provider.origin}), clock=lambda: self.now)
        self.socket = self.root / "open.sock"
        await self.bridge.listen(self.socket)
        install_shim(self.root / "bin")

    async def asyncTearDown(self) -> None:
        await self.bridge.close()
        # Server shutdown blocks for its poll interval, so it runs outside the event loop.
        for tool in self.tools:
            await asyncio.to_thread(tool.close)
        await asyncio.to_thread(self.provider.close)
        shutil.rmtree(self.root)

    def tool(self, style: str) -> FakeTool:
        tool = FakeTool(self.provider, style)
        self.tools.append(tool)
        return tool

    async def launch(self, tool: FakeTool, session: str | None = AGENT) -> subprocess.CompletedProcess[str]:
        environment = {"STUDIO_BROWSER_SOCKET": str(self.socket)}
        if session is not None:
            environment["LAYR_SESSION"] = session
        return await asyncio.to_thread(tool.launch, self.root / "bin", environment)

    async def test_kubelogin_style_link_queues_the_provider_page_after_the_first_hop(self) -> None:
        tool = self.tool("kubelogin")
        result = await self.launch(tool)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Studio asks the person to open 127.0.0.1", result.stderr)
        waited = await self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 0})
        [item] = waited["items"]
        self.assertEqual((item["state"], item["agentId"], item["uid"], item["linuxUser"]),
                         ("pending", AGENT, UID, "studio-a-test"))
        self.assertEqual(item["startUrl"], tool.open_url)
        self.assertTrue(item["url"].startswith(f"http://{self.provider.origin}/authorize?"))
        self.assertEqual(item["redirect"], {"host": "localhost", "port": tool.port, "path": "/"})
        # The first hop read the redirect without cookies and did not use a code.
        self.assertEqual(len(tool.received), 1)
        self.assertNotIn("cookie", tool.received[0][1])
        self.assertEqual(tool.codes, [])

    async def test_aws_style_link_queues_directly_and_the_lease_relays_one_callback(self) -> None:
        tool = self.tool("aws")
        result = await self.launch(tool)
        self.assertEqual(result.returncode, 0, result.stderr)
        [item] = (await self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 0}))["items"]
        self.assertEqual(item["url"], tool.open_url)
        self.assertEqual(item["redirect"], {"host": "127.0.0.1", "port": tool.port, "path": "/oauth/callback"})
        with self.assertRaises(BridgeError):
            await self.bridge.call("browser.callback", {"id": item["id"], "target": "/oauth/callback?code=x"})
        first = await self.bridge.call("browser.answer", {"id": item["id"], "decision": "approve"})
        second = await self.bridge.call("browser.answer", {"id": item["id"], "decision": "approve"})
        self.assertEqual(first, second)
        self.assertEqual(first["lease"]["port"], tool.port)
        with self.assertRaises(BridgeError) as path:
            await self.bridge.call("browser.callback", {"id": item["id"], "target": "/other?code=x"})
        self.assertEqual(path.exception.code, "invalid_params")
        code_url = await asyncio.to_thread(self.follow_provider, item["url"])
        target = code_url.split(f":{tool.port}", 1)[1]
        relayed = await self.bridge.call("browser.callback", {
            "id": item["id"], "target": target,
            "headers": {"user-agent": "test", "cookie": "mac=secret", "authorization": "Bearer mac"}})
        self.assertEqual((relayed["status"], relayed["done"]), (200, True))
        self.assertEqual(len(self.provider.tokens), 1)
        sent = tool.received[-1][1]
        self.assertEqual((sent.get("user-agent"), "cookie" in sent, "authorization" in sent), ("test", False, False))
        self.assertEqual(sent["host"], f"127.0.0.1:{tool.port}")
        with self.assertRaises(BridgeError) as closed:
            await self.bridge.call("browser.callback", {"id": item["id"], "target": target})
        self.assertEqual(closed.exception.code, "stale")
        self.assertEqual(len(self.provider.tokens), 1)

    def follow_provider(self, url: str) -> str:
        import urllib.error
        import urllib.request

        class Stop(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, *args: object, **kwargs: object) -> None:
                return None
        try:
            urllib.request.build_opener(Stop).open(url, timeout=5)
        except urllib.error.HTTPError as error:
            with error:
                return str(error.headers["Location"])
        raise AssertionError("The provider did not redirect")

    async def test_identity_comes_from_the_peer_uid(self) -> None:
        tool = self.tool("aws")
        missing = await self.launch(tool, session=None)
        self.assertEqual(missing.returncode, 1)
        self.assertIn("LAYR_SESSION is missing", missing.stderr)
        unknown = await self.launch(tool, session=str(uuid.uuid4()))
        self.assertIn("does not know the chat", unknown.stderr)
        other_user = await self.launch(tool, session=OTHER)
        self.assertIn("another Linux user", other_user.stderr)
        self.assertEqual((await self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 0}))["items"], [])

    async def test_a_port_of_another_user_and_a_page_without_redirect_are_refused(self) -> None:
        tool = self.tool("aws")
        self.owners[tool.port] = UID + 1
        result = await self.launch(tool)
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"Another Linux user listens on port {tool.port}", result.stderr)
        page = self.tool("aws")
        page.open_url = f"http://localhost:{page.port}/oauth/callback"
        refused = await self.launch(page)
        self.assertIn("does not redirect to a sign-in page", refused.stderr)

    async def test_limits_replace_and_shim_errors(self) -> None:
        tool = self.tool("aws")
        for _ in range(6):
            self.assertEqual((await self.launch(tool)).returncode, 0)
        limited = await self.launch(tool)
        self.assertIn("6 times in 10 minutes", limited.stderr)
        items = (await self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 0}))["items"]
        self.assertEqual([item["state"] for item in items], ["replaced"] * 5 + ["pending"])
        two = subprocess.run([sys.executable, "-I", str(SHIM), "a", "b"], capture_output=True, text=True)
        self.assertEqual(two.returncode, 2)
        absent = subprocess.run([sys.executable, "-I", str(SHIM), "https://example.com/"], capture_output=True,
                                text=True, env={"STUDIO_BROWSER_SOCKET": str(self.root / "none.sock")})
        self.assertEqual(absent.returncode, 1)
        self.assertIn("Studio is not available", absent.stderr)
        raw = await asyncio.open_unix_connection(str(self.socket))
        raw[1].write(b"not json\n")
        self.assertIn(b"invalid_request", await raw[0].readline())
        raw[1].close()

    async def test_wait_wakes_on_a_new_link_and_check_marks_stopped_tools(self) -> None:
        tool = self.tool("aws")
        waiter = asyncio.create_task(self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 10}))
        await asyncio.sleep(0.1)
        self.assertFalse(waiter.done())
        await self.launch(tool)
        woken = await asyncio.wait_for(waiter, 5)
        [item] = woken["items"]
        self.tools.remove(tool)
        await asyncio.to_thread(tool.close)
        await self.bridge.check()
        later = await self.bridge.call("browser.wait", {"after": woken["next"], "waitSeconds": 0})
        self.assertEqual([row["state"] for row in later["items"]], ["stale"])
        with self.assertRaises(BridgeError) as stale:
            await self.bridge.call("browser.answer", {"id": item["id"], "decision": "approve"})
        self.assertEqual(stale.exception.code, "stale")

    async def test_decline_close_expiry_and_device_links(self) -> None:
        tool = self.tool("aws")
        await self.launch(tool)
        [item] = (await self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 0}))["items"]
        self.assertEqual((await self.bridge.call("browser.answer", {"id": item["id"], "decision": "decline"}))["state"],
                         "declined")
        with self.assertRaises(BridgeError):
            await self.bridge.call("browser.answer", {"id": item["id"], "decision": "approve"})
        device = self.tool("aws")
        device.open_url = f"http://{self.provider.origin}/device?user_code=ABCD"
        await self.launch(device)
        items = (await self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 0}))["items"]
        device_item = items[-1]
        self.assertIsNone(device_item["redirect"])
        opened = await self.bridge.call("browser.answer", {"id": device_item["id"], "decision": "approve"})
        self.assertEqual(opened, {"state": "opened", "lease": None})
        aws = self.tool("aws")
        self.contexts[OTHER]["uid"] = UID
        await self.launch(aws, session=OTHER)
        last = (await self.bridge.call("browser.wait", {"after": 0, "waitSeconds": 0}))["items"][-1]
        self.now += 601
        await self.bridge.check()
        self.assertEqual(self.bridge.items[last["id"]]["state"], "expired")
        self.assertEqual((await self.bridge.call("browser.close", {"id": last["id"]}))["state"], "expired")
        # The queue survives a guest service restart.
        async def nobody(agent: str) -> dict[str, object]:
            raise LookupError(agent)
        again = BrowserBridge(self.root / "state", nobody)
        self.assertEqual(set(again.items), set(self.bridge.items))


if __name__ == "__main__":
    unittest.main()


class ServiceMethods(unittest.IsolatedAsyncioTestCase):
    """The browser methods through the guest service's own request path and receipts."""

    async def asyncSetUp(self) -> None:
        from service import Service
        self.root = Path(tempfile.mkdtemp(prefix="sbs-", dir="/tmp"))
        self.provider = FakeProvider()
        self.tool = FakeTool(self.provider, "aws")
        self.service = Service(self.root / "state", self.root / "store", self.root / "projects", self.root / "home",
                               http_providers=frozenset({self.provider.origin}))

        async def context(agent: str) -> dict[str, object]:
            return {"uid": UID, "projectId": "p1", "owner": "studio-a-test"}
        self.service.browser.context = context
        self.service.browser.listeners = lambda: [Listener("127.0.0.1", self.tool.port, UID)]
        await self.service.browser.listen(self.root / "open.sock")
        install_shim(self.root / "bin")

    async def asyncTearDown(self) -> None:
        await self.service.browser.close()
        self.service.close()
        await asyncio.to_thread(self.tool.close)
        await asyncio.to_thread(self.provider.close)
        shutil.rmtree(self.root)

    async def call(self, request_id: str, method: str, params: dict[str, object]) -> dict[str, object]:
        async def emit(event: str, data: object) -> None:
            pass
        response: dict[str, object] = await self.service.request(
            {"id": request_id, "method": method, "params": params}, emit)
        return response

    async def test_receipts_replay_the_answer_and_errors_keep_their_codes(self) -> None:
        result = await asyncio.to_thread(self.tool.launch, self.root / "bin", {
            "STUDIO_BROWSER_SOCKET": str(self.root / "open.sock"), "LAYR_SESSION": AGENT})
        self.assertEqual(result.returncode, 0, result.stderr)
        waited = await self.call("w1", "browser.wait", {"after": 0, "waitSeconds": 0})
        [item] = waited["result"]["items"]
        first = await self.call("a1", "browser.answer", {"id": item["id"], "decision": "approve"})
        self.assertEqual(first["result"]["state"], "approved")
        # The same request ID returns the saved result, also after the state changed.
        await self.call("c1", "browser.close", {"id": item["id"], "reason": "test"})
        self.assertEqual(await self.call("a1", "browser.answer", {"id": item["id"], "decision": "approve"}), first)
        stale = await self.call("a2", "browser.answer", {"id": item["id"], "decision": "approve"})
        self.assertEqual(stale["error"]["code"], "stale")
        missing = await self.call("a3", "browser.callback", {"id": "none", "target": "/"})
        self.assertEqual(missing["error"]["code"], "not_found")
        wrong = await self.call("w2", "browser.wait", {"after": -1})
        self.assertEqual(wrong["error"]["code"], "invalid_params")
