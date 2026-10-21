#!/usr/bin/env python3
"""Sign-in links that an agent gives: the orchestration_sign_in tool and the message rule
(docs/vm-browser-bridge.md, section 7). A real Runtime with a fake app-server, the real host
bridge on a private socket, a fake provider and a fake sign-in command. No model service."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
import shutil
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)

import codex_browser_bridge as bridge_module  # noqa: E402
from codex_browser_bridge import SIGN_IN_CONTRACT, SIGN_IN_TOOL, Bridge, card_id  # noqa: E402
from codex_browser_host import HostGuest  # noqa: E402
from codex_layout import REPOSITORY_ROOT  # noqa: E402
from codex_runtime import TOOLS  # noqa: E402
from browser_bridge import Listener  # noqa: E402
from test_browser_fakes import FakeProvider, FakeTool  # noqa: E402
from test_codex_browser_bridge import browser_get, provider_redirect  # noqa: E402
from studio_api.testing import read_runtime_records  # noqa: E402

UID = os.getuid()
CATALOG = REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-services/src/account_worker/preparation-resources.json"


class Catalog(unittest.TestCase):
    def test_both_catalogs_take_the_tool_from_its_owner(self):
        [python] = [tool for tool in TOOLS if tool["name"] == SIGN_IN_TOOL]
        [rust] = [tool for tool in json.loads(CATALOG.read_text())["tools"] if tool["name"] == SIGN_IN_TOOL]
        self.assertEqual(python, SIGN_IN_CONTRACT["tool"])
        self.assertEqual(rust, SIGN_IN_CONTRACT["tool"])

    def test_the_message_rule_follows_the_shared_corpus(self):
        # The same corpus checks the Rust port (runtime-services browser_policy.rs).
        from codex_layout import VM_GUEST_ROOT
        from browser_policy import sign_in_links
        for case in json.loads((VM_GUEST_ROOT / "sign_in_links.json").read_text()):
            with self.subTest(case["name"]):
                self.assertEqual(sign_in_links(case["text"]), case["links"])

    def test_an_older_bridge_refuses_with_the_contract_message(self):
        import asyncio
        from browser_bridge import BridgeError, BrowserBridge

        async def nobody(agent):
            raise LookupError(agent)
        with tempfile.TemporaryDirectory() as folder:
            async def unknown():
                return await BrowserBridge(Path(folder), nobody).call("browser.unknown", {})
            with self.assertRaises(BridgeError) as refused:
                asyncio.run(unknown())
        self.assertEqual(str(refused.exception), SIGN_IN_CONTRACT["unsupportedMessage"])


class SignInLinks(unittest.TestCase):
    def setUp(self):
        # A short folder: a macOS Unix socket path has at most 103 bytes.
        self.root = Path(tempfile.mkdtemp(prefix="sil-", dir="/tmp"))
        self.runtime = f.make_runtime(f.Runtime, self.root, f.FakeServer)
        self.provider = FakeProvider()
        self.tools = []
        self.guest = HostGuest(self.runtime, listeners=lambda: [Listener("127.0.0.1", tool.port, UID)
                                                                 for tool in self.tools],
                               http_providers=frozenset({self.provider.origin}))
        # The server's own host bridge, with the test provider origin.
        self.runtime._browser_host_bridge = Bridge(self.runtime, lambda: self.guest, source="host",
                                                   http_providers=frozenset({self.provider.origin}))
        self.submits = []
        call = self.guest.call

        def counted(method, params=None, **options):
            if method == "browser.submit":
                self.submits.append(params)
            return call(method, params, **options)
        self.guest.call = counted
        self.lead = f.RuntimeContract.lead(self)

    def tearDown(self):
        self.runtime.close()
        self.runtime._browser_host_bridge.stop()
        self.guest.close()
        for tool in self.tools:
            tool.close()
        self.provider.close()
        shutil.rmtree(self.root, ignore_errors=True)

    def tool(self, style="aws"):
        tool = FakeTool(self.provider, style)
        self.tools.append(tool)
        return tool

    def cards(self):
        return [card for card in read_runtime_records(self.runtime, "requests") if card["method"] == "browser/open"]

    def call(self, request_id, url):
        self.runtime.server.request({"id": request_id, "method": "item/tool/call", "params": {
            "threadId": self.lead["threadId"], "callId": f"sign-in-{request_id}", "tool": SIGN_IN_TOOL,
            "arguments": {"url": url}}})
        f.eventually(lambda: any(reply["id"] == request_id for reply in self.runtime.server.responses))
        reply = next(reply for reply in self.runtime.server.responses if reply["id"] == request_id)
        text = reply["result"]["contentItems"][0]["text"]
        # A refusal is plain text for the model.
        return reply["result"]["success"], json.loads(text) if reply["result"]["success"] else text

    def message(self, item_id, text, thread=None):
        self.runtime.notification({"method": "item/completed", "params": {
            "threadId": thread or self.lead["threadId"], "turnId": self.lead["turnId"],
            "item": {"type": "agentMessage", "id": item_id, "text": text}}},
            "default", self.runtime.connection_ids["default"])

    def test_the_tool_shows_one_card_and_the_browser_reaches_the_command(self):
        command = self.tool()
        ok, result = self.call(301, command.open_url)
        self.assertTrue(ok, result)
        [card] = self.cards()
        self.assertEqual((result["status"], result["requestId"], result["host"]), ("pending", card["id"], card["params"]["host"]))
        self.assertIn("sign-in card", result["message"])
        self.assertEqual((card["agent"], card["params"]["source"], card["params"]["url"]),
                         (self.lead["id"], "host", command.open_url))
        # The same link again, from a retry, the command or the message, keeps the one card.
        ok, again = self.call(302, command.open_url)
        self.message("again", "Open this: " + command.open_url)
        f.eventually(lambda: len(self.submits) == 3)
        self.assertEqual(len(self.guest.bridge.items), 1)
        self.assertEqual(again["requestId"], card["id"])
        self.assertEqual([entry["id"] for entry in self.cards()], [card["id"]])
        answer = self.runtime._browser_host_bridge.answer(card["id"], {"decision": "accept"}, local=True)
        self.assertEqual(answer["openUrl"], command.open_url)
        parts = urlsplit(provider_redirect(answer["openUrl"]))
        status, _headers, _body = browser_get(command.port, (parts.path or "/") + "?" + parts.query)
        self.assertEqual((status, len(command.codes)), (200, 1))

    def test_the_tool_names_why_a_link_cannot_open(self):
        ok, result = self.call(311, "ftp://files.acme.dev/sign-in")
        self.assertFalse(ok)
        self.assertIn("Only https sign-in links", json.dumps(result))
        stopped = self.tool()
        url = stopped.open_url
        self.tools.remove(stopped)
        stopped.close()
        ok, result = self.call(312, url)
        self.assertFalse(ok)
        self.assertIn("no longer listens", json.dumps(result))
        self.assertEqual(self.cards(), [])

    def test_a_sign_in_link_in_a_message_becomes_a_card_and_the_text_stays(self):
        device = f"http://{self.provider.origin}/device?user_code=WXYZ-1234"
        text = f"To sign in, open **{device}** and confirm the code. Docs: https://docs.acme.dev/guide?page=2"
        self.message("answer-1", text)
        f.eventually(lambda: len(self.cards()) == 1)
        [card] = self.cards()
        self.assertEqual((card["agent"], card["params"]["url"], card["params"]["userCode"]),
                         (self.lead["id"], device, "WXYZ-1234"))
        items = read_runtime_records(self.runtime, "items")
        self.assertTrue(any(item.get("text") == text for item in items))

    def test_ordinary_links_reviewers_and_unfinished_messages_get_no_card(self):
        calls = []
        with patch.object(bridge_module, "message_links", lambda *args: calls.append(args)):
            self.message("plain", "See https://github.com/acme/repo/pull/12?diff=split and http://localhost:3000/")
            self.message("doc", "For example https://id.example.com/authorize?client_id=k&response_type=code")
            with self.runtime.lock, self.runtime.db() as db:
                reviewer = self.runtime.agent(self.lead["id"], db)
                reviewer["role"] = "reviewer"
                self.runtime.put(db, "agents", reviewer)
            self.message("review", f"http://{self.provider.origin}/device?user_code=QQQQ")
        self.assertEqual(calls, [])
        # A message that is still streaming, or that is gone, never makes a card.
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, self.lead["id"], "draft", "assistant", "x", streaming=True)
        bridge_module.message_links(self.runtime, self.lead["id"], self.lead["id"] + ":draft",
                                    [f"http://{self.provider.origin}/device?user_code=DRAFT"])
        bridge_module.message_links(self.runtime, self.lead["id"], self.lead["id"] + ":missing",
                                    [f"http://{self.provider.origin}/device?user_code=GONE"])
        self.assertEqual(self.cards(), [])


if __name__ == "__main__":
    unittest.main()
