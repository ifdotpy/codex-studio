#!/usr/bin/env python3
"""Two real runtimes and signed FastAPI routes, isolated from live state.

Run: python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/tests/multi-server-signed-integration.py
Only the HTTPS exchange uses an ASGI adapter. It preserves signed body bytes.
Native HTTP/TLS has a separate suite in studio_api.multi_server.test_transport.
The controlled scheduler permits deterministic dispatch through the real runtime.
"""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
from contextlib import ExitStack
import importlib.util
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from fastapi.testclient import TestClient
from codex_canvas import Canvas
from codex_federation import _crypto, _sign
from codex_multi_server import request_bytes, AccessError
from studio_api.app import create_app
from studio_api.context import ApiContext
from codex_remote import RemoteAccess

spec = importlib.util.spec_from_file_location("signed_fixture", Path(__file__).with_name("worker-defaults-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

OWNER = "owner@example.test"
ROUTE = "/api/servers/orchestration"


class Endpoint:
    def __init__(self, folder, name, port, address):
        self.folder, self.name, self.port, self.address = folder, name, port, address
        self.origin = "https://" + name + ".example.ts.net"
        self.state = folder / "state"
        self.state.mkdir(parents=True)
        (self.state / "remote-access.json").write_text(json.dumps({"enabled": True, "origin": self.origin}))
        self.start()

    def start(self):
        self.runtime = fixture.ControlledRuntime(self.state, fixture.f.FakeServer)
        canvas = Canvas(self.state)
        canvas.runtime = self.runtime
        self.context = ApiContext(canvas, token="test-token", remote=RemoteAccess(self.state), server_port=self.port)
        self.context.api_schema_hash = "test-schema"
        self.app = create_app(self.context)
        self.client = TestClient(self.app, base_url=f"http://127.0.0.1:{self.port}", client=("127.0.0.1", 9999))
        self.server_id = self.runtime.paired_access().local_server_id

    def close(self):
        self.client.close()
        self.context.close()
        self.runtime.close()

    def restart(self):
        old_id = self.server_id
        self.close()
        self.start()
        assert self.server_id == old_id

    def local(self, body):
        response = self.client.post("/api/multi-server", json=body, headers={"X-Canvas-Token": "test-token"})
        if response.status_code != 200:
            raise AssertionError((response.status_code, response.text))
        return response.json()


class SignedIntegration(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        folder = self.stack.enter_context(tempfile.TemporaryDirectory(prefix="studio-signed-integration-"))
        self.folder = Path(folder)
        test_codex_home = self.folder / "codex-home"
        test_codex_home.mkdir()
        tools = self.folder / "bin"
        tools.mkdir()
        self.identity_log = self.folder / "tailscale-calls.jsonl"
        executable = tools / ("tailscale.cmd" if os.name == "nt" else "tailscale")
        stub = tools / "tailscale_stub.py" if os.name == "nt" else executable
        stub.write_text(f'''#!{sys.executable}
import json, os, sys
args = sys.argv[1:]
with open(os.environ["SIGNED_TEST_TAILSCALE_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
if args == ["status", "--json"]:
    value = {{"BackendState": "Running", "Self": {{"UserID": 1}}, "User": {{"1": {{"LoginName": "{OWNER}"}}}}}}
elif len(args) == 3 and args[:2] == ["whois", "--json"] and args[2] in ["100.64.0.11", "100.64.0.12"]:
    value = {{"Node": {{}}, "UserProfile": {{"LoginName": "{OWNER}"}}}}
else:
    sys.exit("The isolated stub refuses this command")
print(json.dumps(value))
''')
        if os.name == "nt":
            executable.write_text(
                f'@echo off\r\n"{sys.executable}" "%~dp0tailscale_stub.py" %*\r\n',
                encoding="utf-8",
            )
        else:
            executable.chmod(0o700)
        self.stack.enter_context(patch.dict(os.environ, {
            "PATH": str(tools) + os.pathsep + os.environ["PATH"],
            "SIGNED_TEST_TAILSCALE_LOG": str(self.identity_log),
            "CODEX_HOME": str(test_codex_home),
            "PYTHONIOENCODING": "utf-8",
            "CODEX_CANVAS_PUBLIC_ORIGIN": "",
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
        }))
        self.a = Endpoint(self.folder / "a", "a", 18765, "100.64.0.11")
        self.stack.callback(self.a.close)
        self.b = Endpoint(self.folder / "b", "b", 18766, "100.64.0.12")
        self.stack.callback(self.b.close)
        self.endpoints = {endpoint.origin: endpoint for endpoint in (self.a, self.b)}
        self.wire = []
        self.wire_lock = threading.Lock()
        self.drop_action = None
        self.stack.enter_context(patch("codex_multi_server._exchange", side_effect=self.exchange))

    def exchange(self, url, method, headers, raw, timeout):
        parsed = urlsplit(url)
        endpoint = self.endpoints[parsed.scheme + "://" + parsed.netloc]
        sender = next(ep for ep in self.endpoints.values() if ep.server_id == headers["X-Studio-Client"])
        target = parsed.path + ("?" + parsed.query if parsed.query else "")
        forwarded = {
            "X-Forwarded-Host": parsed.netloc, "X-Forwarded-Proto": "https",
            "X-Forwarded-For": sender.address, "Tailscale-User-Login": OWNER,
        }
        # Passing content=raw preserves bytes, including JSON order and UTF-8.
        response = endpoint.client.request(method, target, content=raw, headers={**headers, **forwarded})
        record = {"recipient": endpoint.name, "method": method, "target": target,
                  "headers": dict(headers), "raw": raw, "status": response.status_code}
        with self.wire_lock:
            self.wire.append(record)
        if parsed.path == ROUTE and json.loads(raw)["action"] == self.drop_action:
            self.drop_action = None
            raise TimeoutError("The response was lost after the real route ran")
        if parsed.path == "/api/multi-server/v1/name" and getattr(self, "drop_name", False):
            self.drop_name = False
            raise TimeoutError("The rename response was lost after the route ran")
        return {"status": response.status_code, "url": url,
                "body": base64.b64encode(response.content).decode()}

    def tool(self, endpoint, actor, name, arguments, call_id):
        endpoint.runtime.dynamic({"id": call_id, "params": {"threadId": actor["threadId"],
            "callId": call_id, "tool": name, "arguments": arguments}})
        fixture.f.eventually(lambda: any(row["id"] == call_id for row in endpoint.runtime.server.responses))
        response = next(row for row in endpoint.runtime.server.responses if row["id"] == call_id)["result"]
        self.assertTrue(response["success"], response)
        return json.loads(response["contentItems"][0]["text"])

    def drain(self):
        for endpoint in (self.a, self.b):
            service = endpoint.runtime.multi_server()
            with endpoint.runtime.read_db() as db:
                queued = [row[0] for row in db.execute("SELECT id FROM runtime_server_outbox WHERE state='queued' ORDER BY rowid")]
            for key in queued:
                service.deliver(key)

    def drive(self, predicate, timeout=12):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if predicate():
                return
            self.b.runtime.dispatch()
            time.sleep(0.01)
        self.fail("The signed runtime flow did not reach its expected state")

    def git(self, directory, *arguments):
        return subprocess.check_output(["git", "-C", str(directory), *arguments], text=True, timeout=15).strip()

    def events(self, endpoint, agent, kind=None, text=None):
        with endpoint.runtime.read_db() as db:
            rows = db.execute("SELECT id,kind,text FROM runtime_events WHERE agent=?", (agent,)).fetchall()
        return [dict(row) for row in rows if (kind is None or row["kind"] == kind) and (text is None or row["text"] == text)]

    def actions(self, name):
        return [row for row in self.wire if row["target"] == ROUTE and json.loads(row["raw"])["action"] == name]

    def test_signed_server_name_and_peer_status_propagation(self):
        invitation = self.a.local({"action": "create_invite", "requestId": "invite-names"})["invitation"]
        self.b.local({"action": "accept_invite", "invitation": invitation, "requestId": "pair-names"})
        a = self.a.runtime.paired_access()
        b = self.b.runtime.paired_access()
        aliases_a, aliases_b = a.server_aliases(), b.server_aliases()
        keys_a, keys_b = a._keys().copy(), b._keys().copy()
        request = {"action": "name", "serverId": self.b.server_id, "label": "Kukuka Windows", "requestId": "remote-name"}
        state = self.a.local(request)
        self.assertEqual(b.identity()["label"], "Kukuka Windows")
        self.assertEqual(state["servers"][0]["label"], "Kukuka Windows")
        self.a.local(request)
        b.rename("Newer Windows", "newer-name", "local")
        self.assertEqual(self.a.local(request)["servers"][0]["label"], "Newer Windows")
        self.a.local({"action": "name", "serverId": "local", "label": "Lumina Mac", "requestId": "local-name"})
        # Discovery verifies a paired peer through its signed status endpoint.
        a_identity = {"protocol": 1, **a.identity(owner=True), "autoPair": True}
        with patch.object(b.discovery(), "probe", return_value=a_identity):
            b.discovery()._candidate(self.a.origin, OWNER, time.monotonic() + 10)
        self.assertEqual(b.servers()[0]["label"], "Lumina Mac")
        self.assertEqual(a.server_aliases(), aliases_a)
        self.assertEqual(b.server_aliases(), aliases_b)
        for service, keys in ((a, keys_a), (b, keys_b)):
            self.assertEqual(service._keys()["publicKey"], keys["publicKey"])
            self.assertEqual(service._keys()["privateKey"], keys["privateKey"])
        names = [row for row in self.wire if row["target"] == "/api/multi-server/v1/name"]
        self.assertEqual(len(names), 1)
        self.assertEqual(names[0]["status"], 200)
        self.assertTrue(any(row["target"] == "/api/multi-server/v1/status" and row["status"] == 200 for row in self.wire))
        with self.assertRaises(AssertionError):
            self.a.local({**request, "label": "Conflicting"})
        # A signed response with different keys cannot change the stored label.
        with self.assertRaises(AccessError):
            a.refresh_peer_identity(self.b.server_id, {**b.identity(), "publicKey": keys_a["publicKey"], "label": "Wrong"})
        self.assertEqual(a.servers()[0]["label"], "Newer Windows")
        unsigned = {"protocol": 1, **b.identity(owner=True), "autoPair": True, "label": "Unsigned name"}
        original_exchange = self.exchange
        def old_server_exchange(url, method, headers, raw, timeout):
            if url.endswith("/api/multi-server/v1/status"):
                return {"status": 404, "url": url, "body": base64.b64encode(b'{"error":"Not found"}').decode()}
            return original_exchange(url, method, headers, raw, timeout)
        with patch.object(a.discovery(), "probe", return_value=unsigned), patch("codex_multi_server._exchange", side_effect=old_server_exchange):
            a.discovery()._candidate(self.b.origin, OWNER, time.monotonic() + 10)
        self.assertEqual(a.servers()[0]["label"], "Newer Windows")
        self.assertEqual(a.servers()[0]["reachability"], "reachable")
        self.drop_name = True
        lost = {**request, "label": "Recovered Windows", "requestId": "lost-rename"}
        with self.assertRaises(AssertionError):
            self.a.local(lost)
        self.assertEqual(b.identity()["label"], "Recovered Windows")
        self.assertEqual(self.a.local(lost)["servers"][0]["label"], "Recovered Windows")
        with self.b.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_audit WHERE action='name'").fetchone()[0], 3)

    def test_pair_spawn_native_turn_retry_task_fetch_stop_and_revocation(self):
        invitation = self.a.local({"action": "create_invite", "requestId": "invite-a"})["invitation"]
        self.b.local({"action": "accept_invite", "invitation": invitation, "requestId": "accept-b"})
        # A accepted B's proof; B pins A's invitation identity. Both directions use real keys.
        self.assertEqual(self.a.runtime.paired_access().servers()[0]["id"], self.b.server_id)
        self.assertEqual(self.b.runtime.paired_access().servers()[0]["id"], self.a.server_id)
        self.assertEqual(self.wire[0]["target"], "/api/multi-server/v1/pair")
        self.assertEqual(self.wire[0]["status"], 200)
        identity_calls = [json.loads(line) for line in self.identity_log.read_text().splitlines()]
        self.assertIn(["status", "--json"], identity_calls)
        self.assertIn(["whois", "--json", self.b.address], identity_calls)

        repo = self.b.folder / "project"
        repo.mkdir()
        self.git(repo, "init", "-q", "-b", "worker")
        self.git(repo, "config", "user.name", "Fixture")
        self.git(repo, "config", "user.email", "fixture@example.test")
        (repo / "base-blob").write_bytes(secrets.token_bytes(1024 * 1024))
        self.git(repo, "add", "base-blob")
        self.git(repo, "commit", "-qm", "Base")
        base = self.git(repo, "rev-parse", "HEAD")
        destination = self.a.folder / "project"
        subprocess.run(["git", "clone", "-q", str(repo), str(destination)], check=True, timeout=15)
        lead = self.a.runtime.create({"name": "Lead", "prompt": "", "cwd": str(destination), "concurrency": 4}, draft=True)
        lead = self.a.runtime.prepare(lead)
        task = self.tool(self.a, lead, "orchestration_task", {"action": "create", "title": "Check source"}, "task-create")
        args = {"request_id": "spawn-worker", "server": self.b.server_id, "agents": [{
            "name": "Worker", "prompt": "Inspect café source", "cwd": str(repo), "role": "reviewer", "task_id": task["id"]}]}
        self.drop_action = "spawn"
        spawn = self.tool(self.a, lead, "orchestration_spawn", args, "spawn-worker")
        self.assertEqual(spawn["outcome"], "unknown")
        worker_id = spawn["agents"][0]["id"]
        self.assertEqual(self.b.runtime.agent(worker_id)["remoteOrigin"]["home"], self.a.server_id)
        self.assertEqual(self.a.runtime.agent(worker_id)["remoteWorker"]["server"], self.b.server_id)
        with self.b.runtime.read_db() as db:
            link = self.b.runtime.multi_server().link(db, self.b.runtime.agent(worker_id)["remoteOrigin"]["link"])
        self.assertEqual(link["parent"], lead["id"])
        self.drive(lambda: bool(self.b.runtime.agent(worker_id).get("turnId")))
        first_turn = self.b.runtime.agent(worker_id)
        self.assertTrue(first_turn["inFlight"])
        starts = [params for method, params in self.b.runtime.server.calls if method == "turn/start"]
        self.assertEqual(len(starts), 1)

        # Submit through the worker's native dynamic tool, not a direct service call.
        submitted = self.tool(self.b, first_turn, "orchestration_task", {
            "action": "submit", "task_id": task["id"], "result": "Checked café source", "checks": "Fixture", "revision": base}, "worker-submit")
        self.assertEqual(submitted["status"], "review")
        self.b.runtime.server.complete(first_turn["threadId"], first_turn["turnId"], "Checked café source")
        self.drive(lambda: len(self.events(self.a, lead["id"], "child_result")) == 1)
        child = json.loads(self.events(self.a, lead["id"], "child_result")[0]["text"])
        self.assertEqual(child["result"], "Checked café source")
        self.assertEqual(child["agent_id"], worker_id)

        self.tool(self.a, lead, "orchestration_send", {
            "agent_id": worker_id, "text": "Inspect follow-up", "request_id": "input-one"}, "input-one")
        self.drop_action = "input"
        self.drive(lambda: bool(self.b.runtime.agent(worker_id).get("turnId")))
        input_starts = len([1 for method, _ in self.b.runtime.server.calls if method == "turn/start"])
        self.assertEqual(input_starts, 2)
        self.assertEqual(len(self.events(self.b, worker_id, text="Inspect follow-up")), 1)

        # Restart A with the same state. Retry the same spawn and input identities.
        self.a.restart()
        lead = self.a.runtime.prepare(self.a.runtime.agent(lead["id"]))
        repeated = self.tool(self.a, lead, "orchestration_spawn", args, "spawn-worker-retry")
        self.assertEqual(repeated["agents"][0]["id"], worker_id)
        self.tool(self.a, lead, "orchestration_send", {
            "agent_id": worker_id, "text": "Inspect follow-up", "request_id": "input-one"}, "input-one-retry")
        self.drain()
        with self.b.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_agents WHERE json_type(record,'$.remoteOrigin')='object'").fetchone()[0], 1)
        self.assertEqual(len(self.events(self.b, worker_id, text="Inspect follow-up")), 1)
        self.assertEqual(len([1 for method, _ in self.b.runtime.server.calls if method == "turn/start"]), input_starts)
        for action in ("spawn", "input"):
            attempts = self.actions(action)
            self.assertGreaterEqual(len(attempts), 2, action)
            self.assertEqual(len({row["headers"]["X-Studio-Request-Id"] for row in attempts}), 1)
            self.assertEqual(len({row["raw"] for row in attempts}), 1)
            self.assertEqual(len({row["headers"]["X-Studio-Nonce"] for row in attempts}), len(attempts))
        with self.a.runtime.read_db() as db:
            submitted_task = self.a.runtime.work_by_id(db, task["id"], lead["rootId"])
        self.assertEqual(submitted_task["status"], "review")
        self.assertEqual(submitted_task["owner"], worker_id)
        self.assertEqual(len(submitted_task["results"]), 1)
        self.assertEqual(submitted_task["results"][0]["text"], "Checked café source")

        (repo / "new-blob").write_bytes(secrets.token_bytes(350 * 1024))
        self.git(repo, "add", "new-blob")
        self.git(repo, "commit", "-qm", "Worker result")
        tip = self.git(repo, "rev-parse", "HEAD")
        fetched = self.tool(self.a, lead, "orchestration_servers", {
            "action": "fetch", "server": self.b.server_id, "agent_id": worker_id,
            "branch": "worker", "destination": str(destination)}, "fetch-worker")
        self.assertEqual(fetched["commit"], tip)
        self.assertEqual(self.git(destination, "rev-parse", "FETCH_HEAD"), tip)
        self.assertEqual(self.git(destination, "rev-parse", "HEAD"), base)
        self.assertGreater(fetched["bytes"], 256 * 1024)
        self.assertLess(fetched["bytes"], 512 * 1024)
        export = json.loads(self.actions("export")[-1]["raw"])
        self.assertIn(base, export["payload"]["known"])
        self.assertGreaterEqual(len(self.actions("chunk")), 3)
        self.assertEqual(list((self.b.state / "server-exports").glob("*.bundle")), [])

        self.tool(self.a, lead, "orchestration_interrupt", {"agent_id": worker_id}, "stop-worker")
        self.drain()
        self.assertFalse(self.b.runtime.agent(worker_id)["autoWake"])
        self.assertFalse(self.a.runtime.agent(worker_id)["autoWake"])
        self.assertTrue(any(method == "turn/interrupt" for method, _ in self.b.runtime.server.calls))

        # Each network operation had a signature and reached the real receiver inbox.
        for action in ("spawn", "admit", "task", "event", "input", "export", "chunk", "release", "stop"):
            records = self.actions(action)
            self.assertTrue(records, action)
            for row in records:
                envelope = json.loads(row["raw"])
                self.assertEqual(row["headers"]["X-Studio-Request-Id"], envelope["requestId"])
                self.assertTrue(row["headers"]["X-Studio-Signature"])
                self.assertEqual(row["status"], 200)
                endpoint = next(ep for ep in self.endpoints.values() if ep.name == row["recipient"])
                with endpoint.runtime.read_db() as db:
                    saved = db.execute("SELECT state FROM runtime_server_inbox WHERE id=?", (envelope["requestId"],)).fetchone()
                self.assertIsNotNone(saved, action)
                self.assertEqual(saved["state"], "complete")

        self.assert_unauthorized_has_no_effect()

    def assert_unauthorized_has_no_effect(self):
        keys = _crypto("generate")
        raw = json.dumps({"requestId": "unpaired-attempt", "action": "spawn", "payload": {}}).encode()
        timestamp, nonce = str(int(time.time())), secrets.token_urlsafe(24)
        signature = _sign(keys["privateKey"], request_bytes("POST", ROUTE, self.b.server_id,
                          "unknown-server", timestamp, nonce, "unpaired-attempt", raw))
        headers = {"X-Studio-Client": "unknown-server", "X-Studio-Server": self.b.server_id,
                   "X-Studio-Timestamp": timestamp, "X-Studio-Nonce": nonce,
                   "X-Studio-Request-Id": "unpaired-attempt", "X-Studio-Signature": signature}
        with self.b.runtime.read_db() as db:
            before = db.execute("SELECT count(*) FROM runtime_server_inbox").fetchone()[0]
        proxy = {"X-Forwarded-Host": "b.example.ts.net", "X-Forwarded-Proto": "https",
                 "X-Forwarded-For": self.a.address, "Tailscale-User-Login": OWNER, "Content-Type": "application/json"}
        response = self.b.client.post(ROUTE, content=raw, headers={**headers, **proxy})
        self.assertEqual(response.status_code, 403, response.text)
        self.b.local({"action": "revoke", "clientId": self.a.server_id, "requestId": "revoke-a"})
        raw = json.dumps({"requestId": "revoked-attempt", "action": "spawn", "payload": {}}).encode()
        headers = self.a.runtime.paired_access().signed_headers(self.b.server_id, "POST", ROUTE, raw, "revoked-attempt")
        response = self.b.client.post(ROUTE, content=raw, headers={**headers, **proxy})
        self.assertEqual(response.status_code, 403, response.text)
        with self.b.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_server_inbox").fetchone()[0], before)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_agents WHERE json_type(record,'$.remoteOrigin')='object'").fetchone()[0], 1)


if __name__ == "__main__":
    unittest.main()
