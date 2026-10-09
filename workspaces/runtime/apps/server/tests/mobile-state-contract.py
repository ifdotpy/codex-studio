#!/usr/bin/env python3
"""Entity sync keeps work details on the dedicated work resource."""
from codex_layout import REPOSITORY_ROOT, WEB_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import importlib.util
import json
from pathlib import Path
import threading
import time
import unittest
from unittest.mock import patch
import urllib.error
import urllib.request

spec = importlib.util.spec_from_file_location(
    "mobile_state_fixture", Path(__file__).with_name("workspace-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from studio_api.testing import read_runtime_state
from codex_canvas import Canvas, make_server


class MobileStateContract(unittest.TestCase):
    setUp = fixture.WorkspaceContract.setUp
    tearDown = fixture.WorkspaceContract.tearDown
    lead = fixture.WorkspaceContract.lead
    agent_update = fixture.WorkspaceContract.agent_update

    def saved_work(self):
        with self.runtime.db() as db:
            return [tuple(row) for row in db.execute(
                "SELECT id,record FROM runtime_work ORDER BY id"
            )]

    def test_large_history_stays_available_without_entering_chat_updates(self):
        lead = self.lead()
        other = self.lead("Other team")
        count = 620
        with self.runtime.db() as db:
            for number in range(count):
                self.runtime.put(db, "work", {
                    "id": f"work-{number}", "rootId": lead["id"],
                    "title": f"Task {number}", "owner": lead["id"],
                    "description": "Original instructions. " * 30,
                    "status": "review", "dependencies": [],
                    "created": number, "updated": number, "version": 7,
                    "results": [{
                        "id": f"evidence-{number}", "agent": lead["id"],
                        "text": "Полное доказательство 🚀. " * 120,
                        "checks": "Exact test evidence. " * 30,
                        "revision": "exact-source-revision",
                        "files": ["src/retained.py"], "created": number,
                    }],
                    "decisions": [{
                        "resultId": f"evidence-{number}", "decision": "reject",
                        "reason": "Required correction. " * 30,
                        "by": "user", "created": number,
                    }],
                })
        saved = self.saved_work()
        full = read_runtime_state(self.runtime)
        chat = read_runtime_state(self.runtime, include_work=False)
        self.assertNotIn("work", chat)
        self.assertEqual(chat, {key: value for key, value in full.items() if key != "work"})
        self.assertEqual(len(full["work"]), count)
        full_bytes = len(json.dumps(full, ensure_ascii=False).encode())
        chat_bytes = len(json.dumps(chat, ensure_ascii=False).encode())
        self.assertGreater(full_bytes, chat_bytes)
        self.assertLess(full_bytes, 100_000)

        # This is the existing /api/work handler used when the Work screen opens.
        details = self.runtime.work_action(lead["id"], {"action": "list"})
        self.assertEqual(details["tasks"], details["items"])
        self.assertEqual({item["id"] for item in details["items"]},
                         {item["id"] for item in full["work"]})
        retained = next(item for item in details["items"] if item["id"] == "work-0")
        self.assertEqual(retained["description"], "Original instructions. " * 30)
        self.assertGreater(len(json.dumps(details, ensure_ascii=False).encode()), 4_000_000)
        self.assertEqual(self.runtime.work_action(other["id"], {})["items"], [])
        self.assertEqual(self.saved_work(), saved)

    def test_deleted_team_work_records_remain_in_entity_store(self):
        lead = self.lead()
        task = self.runtime.work_action(lead["id"], {
            "action": "create", "title": "Retained history",
        })
        self.assertEqual(read_runtime_state(self.runtime)["work"][0]["id"], task["id"])
        self.agent_update(lead, deletedAt=1)
        self.assertEqual([item["id"] for item in read_runtime_state(self.runtime)["work"]],
                         [task["id"]])
        self.assertNotIn("work", read_runtime_state(self.runtime, include_work=False))
        with self.assertRaisesRegex(ValueError, "deleted"):
            self.runtime.work_action(lead["id"], {"action": "list"})
        self.assertEqual(len(self.saved_work()), 1)


class MobileStateHttpContract(unittest.TestCase):
    lead = fixture.WorkspaceContract.lead
    agent_update = fixture.WorkspaceContract.agent_update

    def setUp(self):
        fixture.WorkspaceContract.setUp(self)
        self.canvas = Canvas(self.state)
        self.canvas.runtime = self.runtime
        self.static = self.root / "web"
        self.static.mkdir()
        self.worker = (WEB_ROOT / "public/studio-sw.js").read_bytes()
        (self.static / "studio-sw.js").write_bytes(self.worker)
        (self.static / "private.js").write_text("This file must stay unavailable.")
        self.web_patch = patch("codex_canvas.WEB", self.static)
        self.web_patch.start()
        self.start_server()

    def start_server(self):
        self.server = make_server(self.canvas)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def stop_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        self.assertFalse(self.thread.is_alive())

    def tearDown(self):
        self.stop_server()
        self.web_patch.stop()
        fixture.WorkspaceContract.tearDown(self)

    def request(self, path, headers=None):
        request = urllib.request.Request(self.url + path, headers=headers or {})
        try:
            response = urllib.request.urlopen(request, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            body = response.read()
            return response.status, response.headers, body

    def get(self, path):
        status, _, body = self.request(path)
        self.assertEqual(status, 200, body)
        return json.loads(body)

    def test_entity_sync_keeps_work_details_on_dedicated_endpoint(self):
        lead = self.lead()
        work = self.runtime.work_action(lead["id"], {
            "action": "create", "title": "Exact evidence",
            "description": "Saved instructions " * 200,
        })
        identity = self.get("/api/sync/identity")
        self.assertNotIn("chatState", identity)
        pull = self.get("/api/sync/pull?scope=state:entities:v1&limit=500")
        self.assertEqual(pull["workspaceId"], identity["workspaceId"])
        entities = [json.loads(row["payload"]) for row in pull["documents"]]
        work_entity = next(entity["value"] for entity in entities
                           if entity["collection"] == "work" and entity["id"] == work["id"])
        self.assertNotIn("description", work_entity)
        self.assertNotIn("results", work_entity)
        details = self.get("/api/work?agent=" + lead["id"])
        self.assertEqual(details["items"][0]["description"], work["description"])

    def test_checkpoint_and_workspace_survive_adapter_reload(self):
        lead = self.lead()
        identity = self.get("/api/sync/identity")
        initial = self.get("/api/sync/pull?scope=state:entities:v1")
        checkpoint = initial["checkpoint"]["seq"]
        self.assertGreater(checkpoint, 0)
        unchanged = self.get(f"/api/sync/pull?scope=state:entities:v1&after={checkpoint}")
        self.assertEqual(unchanged["documents"], [])
        self.assertEqual(unchanged["checkpoint"], {"seq": checkpoint})
        self.stop_server()
        self.start_server()
        self.assertEqual(self.get("/api/sync/identity"), identity)
        self.assertEqual(
            self.get(f"/api/sync/pull?scope=state:entities:v1&after={checkpoint}")["documents"], []
        )
        self.agent_update(lead, name="Updated after reload")
        changed = self.get(f"/api/sync/pull?scope=state:entities:v1&after={checkpoint}")
        self.assertGreater(changed["checkpoint"]["seq"], checkpoint)
        self.assertEqual(changed["workspaceId"], identity["workspaceId"])
        payload = json.loads(changed["documents"][0]["payload"])
        self.assertEqual(payload["value"]["name"], "Updated after reload")

    def test_workspace_resources_support_conditional_gets(self):
        lead = self.lead()
        agent = lead["id"]
        paths = [
            f"/api/workspace?agent={agent}",
            f"/api/changes?agent={agent}&scope=chat",
            f"/api/plan?agent={agent}",
            f"/api/checkpoints?agent={agent}",
            f"/api/capabilities?agent={agent}",
            "/api/rules?agent=" + agent,
        ]
        validators = {}
        for path in paths:
            status, headers, body = self.request(path)
            self.assertEqual(status, 200, path)
            self.assertTrue(body, path)
            validator_pattern = r'^(?:W/)?"[0-9a-f]{64}"$'
            self.assertRegex(headers.get("ETag", ""), validator_pattern, path)
            validators[path] = headers["ETag"]
            refreshed_at = None
            if path.startswith("/api/capabilities?"):
                self.assertTrue(headers["ETag"].startswith("W/"))
                refreshed_at = self.runtime.capability_cache[agent]["at"]
                self.runtime.capability_cache[agent]["at"] -= 31
            unchanged, unchanged_headers, unchanged_body = self.request(
                path, {"If-None-Match": headers["ETag"]}
            )
            self.assertEqual(unchanged, 304, path)
            self.assertEqual(unchanged_headers["ETag"], headers["ETag"], path)
            self.assertEqual(unchanged_body, b"", path)
            if refreshed_at is not None:
                self.assertGreater(self.runtime.capability_cache[agent]["at"], refreshed_at)
        self.runtime.plan_action(agent, {"version": 0, "text": "Changed plan"})
        path = f"/api/plan?agent={agent}"
        changed, headers, body = self.request(
            path, {"If-None-Match": validators[path]}
        )
        self.assertEqual(changed, 200)
        self.assertNotEqual(headers["ETag"], validators[path])
        self.assertEqual(json.loads(body)["text"], "Changed plan")

    def test_workspace_parts_and_incremental_tasks_have_independent_validators(self):
        lead = self.lead()
        worker = fixture.WorkspaceContract.worker(self, lead, "Workspace worker")
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "tasks", {
                "id": "first-command", "agent": worker["id"], "status": "running",
                "kind": "command",
                "created": 100, "command": "first", "processId": "1",
            })
        work_path = f"/api/workspace?agent={lead['id']}&view=work"
        work_status, work_headers, work_body = self.request(work_path)
        self.assertEqual(work_status, 200)
        self.assertEqual(json.loads(work_body), {"work": []})
        inbox_path = f"/api/workspace?agent={lead['id']}&view=inbox"
        annotation_path = f"/api/workspace?agent={lead['id']}&view=annotations"
        inbox_status, inbox_headers, inbox_body = self.request(inbox_path)
        annotation_status, annotation_headers, annotation_body = self.request(annotation_path)
        self.assertEqual((inbox_status, annotation_status), (200, 200))
        self.assertNotEqual(inbox_headers["ETag"], annotation_headers["ETag"])
        self.assertNotEqual(json.loads(inbox_body), json.loads(annotation_body))

        initial_path = f"/api/workspace/tasks?agent={lead['id']}"
        initial_status, initial_headers, initial_body = self.request(initial_path)
        self.assertEqual(initial_status, 200)
        initial = json.loads(initial_body)
        self.assertEqual([task["id"] for task in initial["tasks"]], ["first-command"])
        self.assertNotIn("tail", initial["tasks"][0])

        now = time.time() + 1
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "tasks", {
                "id": "second-command", "agent": worker["id"], "status": "running",
                "kind": "command",
                "created": now, "command": "second", "processId": "2",
            })
        unchanged_status, unchanged_headers, unchanged_body = self.request(
            work_path, {"If-None-Match": work_headers["ETag"]}
        )
        self.assertEqual(unchanged_status, 304)
        self.assertEqual(unchanged_body, b"")
        unchanged_inbox, _, unchanged_inbox_body = self.request(
            inbox_path, {"If-None-Match": inbox_headers["ETag"]}
        )
        self.assertEqual(unchanged_inbox, 304)
        self.assertEqual(unchanged_inbox_body, b"")

        from urllib.parse import quote
        cursor_path = initial_path + "&cursor=" + quote(json.dumps(initial["cursor"]))
        changed_status, changed_headers, changed_body = self.request(cursor_path)
        self.assertEqual(changed_status, 200)
        changed = json.loads(changed_body)
        self.assertEqual([task["id"] for task in changed["tasks"]], ["second-command"])
        latest_path = initial_path + "&cursor=" + quote(json.dumps(changed["cursor"]))
        latest_status, latest_headers, latest_body = self.request(latest_path)
        self.assertEqual(latest_status, 200)
        self.assertEqual(json.loads(latest_body)["tasks"], [])
        no_change_status, _, no_change_body = self.request(
            latest_path, {"If-None-Match": latest_headers["ETag"]}
        )
        self.assertEqual(no_change_status, 304)
        self.assertEqual(no_change_body, b"")

    def test_graph_alias_does_not_change_canonical_runtime_parent(self):
        lead = self.agent_update(self.lead(), threadId="exact-native-thread")
        with self.canvas.connect() as db:
            db.execute("INSERT INTO graph_agents VALUES (?,?)", ("alias", json.dumps({
                "id": "alias", "threadId": lead["threadId"],
                "parentId": "graph-parent", "name": "Graph alias",
            })))
        canonical = read_runtime_state(self.runtime)["agents"]
        canonical_agent = next(agent for agent in canonical if agent["id"] == lead["id"])
        graph = next(agent for agent in self.canvas.threads() if agent["id"] == lead["id"])
        self.assertEqual(canonical_agent["parentId"], lead["parentId"])
        self.assertNotIn("graphAlias", canonical_agent)
        self.assertEqual(graph["parentId"], "graph-parent")
        self.assertEqual(graph["graphAlias"], "alias")

    def test_invalid_scope_never_returns_or_stores_a_state_document(self):
        self.lead()
        self.get("/api/sync/identity")
        for scope in ("state", "state:chat", "state:unknown", "state:entities:v2"):
            status, _, body = self.request("/api/sync/pull?scope=" + scope)
            self.assertEqual(status, 400)
            self.assertEqual(json.loads(body), {"error": "Invalid sync scope"})
        with self.canvas.connect() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM sync_documents").fetchone()[0], 0)

    def test_worker_script_cache_policy_and_exact_origin_gate(self):
        status, headers, body = self.request("/studio-sw.js")
        self.assertEqual(status, 200)
        self.assertEqual(body, self.worker)
        self.assertEqual(headers["Content-Type"], "text/javascript; charset=utf-8")
        self.assertEqual(headers["Cache-Control"], "no-cache")
        self.assertEqual(headers["X-Content-Type-Options"], "nosniff")
        self.assertEqual(self.request("/private.js")[0], 404)
        origin = "https://fixture.example.ts.net"
        remote_headers = {
            "X-Forwarded-Host": "fixture.example.ts.net", "X-Forwarded-Proto": "https",
            "X-Forwarded-For": "100.100.12.34", "Origin": origin,
        }
        paths = ("/studio-sw.js", "/api/sync/identity",
                 "/api/sync/pull?scope=state:entities:v1")
        for path in paths:
            self.assertEqual(self.request(path, remote_headers)[0], 403)
        config = self.canvas.root / "remote-access.json"
        config.write_text(json.dumps({"enabled": True, "origin": origin}))
        for path in paths:
            self.assertEqual(self.request(path, remote_headers)[0], 200)
            for invalid in ({"Origin": "https://evil.example"},
                            {"Sec-Fetch-Site": "cross-site"},
                            {"X-Forwarded-For": "192.168.1.10"},
                            {"Host": "evil.example"}):
                self.assertEqual(self.request(path, {**remote_headers, **invalid})[0], 403)
        config.write_text('{"enabled":false}')
        for path in paths:
            self.assertEqual(self.request(path, remote_headers)[0], 403)


if __name__ == "__main__":
    unittest.main()
