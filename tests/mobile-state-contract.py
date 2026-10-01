#!/usr/bin/env python3
"""Chat snapshots exclude work history without changing its durable records."""

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
import codex_runtime
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
        full = self.runtime.snapshot()
        original_records = self.runtime.records

        def records_without_work(db, table):
            if table == "work":
                self.fail("A chat snapshot loaded work history under the runtime lock")
            return original_records(db, table)

        with patch.object(self.runtime, "records", side_effect=records_without_work):
            chat = self.runtime.snapshot(include_work=False)
        self.assertNotIn("work", chat)
        self.assertEqual(chat, {key: value for key, value in full.items() if key != "work"})
        self.assertEqual(len(full["work"]), count)
        full_bytes = len(json.dumps(full, ensure_ascii=False).encode())
        chat_bytes = len(json.dumps(chat, ensure_ascii=False).encode())
        self.assertGreater(full_bytes, 4_000_000)
        self.assertLess(chat_bytes, full_bytes / 100)

        # This is the existing /api/work handler used when the Work screen opens.
        details = self.runtime.work_action(lead["id"], {"action": "list"})
        self.assertEqual(details["tasks"], details["items"])
        self.assertEqual(
            [{key: value for key, value in item.items()
              if key not in {"blockedBy", "displayStatus"}} for item in details["items"]],
            full["work"],
        )
        self.assertEqual(self.runtime.work_action(other["id"], {})["items"], [])
        self.assertEqual(self.saved_work(), saved)

    def test_default_snapshot_preserves_deleted_team_filter(self):
        lead = self.lead()
        task = self.runtime.work_action(lead["id"], {
            "action": "create", "title": "Retained history",
        })
        self.assertEqual(self.runtime.snapshot()["work"][0]["id"], task["id"])
        self.agent_update(lead, deletedAt=1)
        self.assertEqual(self.runtime.snapshot()["work"], [])
        self.assertNotIn("work", self.runtime.snapshot(include_work=False))
        with self.assertRaisesRegex(ValueError, "deleted"):
            self.runtime.work_action(lead["id"], {"action": "list"})
        self.assertEqual(len(self.saved_work()), 1)

    def test_snapshot_uses_captured_connection_map_for_notice_visibility(self):
        self.runtime.connection_ids["default"] = "connection-before"
        with self.runtime.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_native_notices(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            self.runtime.put(db, "native_notices", {
                "id": "notice", "accountKey": "default",
                "connectionId": "connection-before", "message": "Visible",
            })
        original = codex_runtime.account_notices

        def replace_connection_after_snapshot_lock(runtime_view, db):
            self.runtime.connection_ids["default"] = "connection-after"
            return original(runtime_view, db)

        with patch("codex_runtime.account_notices", side_effect=replace_connection_after_snapshot_lock):
            snapshot = self.runtime.snapshot()
        self.assertEqual([notice["id"] for notice in snapshot["nativeNotices"]], ["notice"])

    def test_snapshot_defers_while_connection_map_is_locked(self):
        with self.runtime.start_lock:
            self.runtime.connection_ids["default"] = "stable-connection"
        with self.runtime.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_native_notices(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            self.runtime.put(db, "native_notices", {
                "id": "deferred-notice", "accountKey": "default",
                "connectionId": "stable-connection", "message": "Visible after retry",
            })

        with self.runtime.db() as probe:
            before = probe.execute("PRAGMA data_version").fetchone()[0]
            self.runtime.start_lock.acquire()
            try:
                with self.assertRaisesRegex(codex_runtime.SnapshotDeferred, "retry shortly"):
                    self.runtime.snapshot()
            finally:
                self.runtime.start_lock.release()

            snapshot = self.runtime.snapshot()
            after = probe.execute("PRAGMA data_version").fetchone()[0]
        self.assertEqual([notice["id"] for notice in snapshot["nativeNotices"]], ["deferred-notice"])
        self.assertEqual(after, before)

    def test_snapshot_preserves_effective_rate_limit_account_fallbacks(self):
        self.runtime.rate_limits = {"accountKey": "default", "data": {"effective": True}}
        self.runtime.rate_limits_by_account = {
            "default": {"accountKey": "default", "data": {"stale": True}},
            "secondary": {"accountKey": "secondary", "data": {"secondary": True}},
        }
        snapshot = self.runtime.snapshot()
        self.assertEqual(snapshot["rateLimits"], self.runtime.rate_limits)
        self.assertEqual(snapshot["rateLimitsByAccount"]["default"], self.runtime.rate_limits)
        self.assertEqual(snapshot["rateLimitsByAccount"]["secondary"],
                         self.runtime.rate_limits_by_account["secondary"])

    def test_snapshot_does_not_hold_start_lock_waiting_for_runtime_lock(self):
        finished = threading.Event()
        outcomes = []

        def snapshot():
            try:
                self.runtime.snapshot()
                outcomes.append("unexpected success")
            except codex_runtime.SnapshotDeferred:
                outcomes.append("deferred")
            finally:
                finished.set()

        worker = threading.Thread(target=snapshot, daemon=True)
        with self.runtime.lock:
            worker.start()
            completed_while_locked = finished.wait(1)
            # A sender holding Runtime.lock must still be able to enter connect.
            start_available = self.runtime.start_lock.acquire(blocking=False)
            if start_available:
                self.runtime.start_lock.release()
        worker.join(2)
        self.assertFalse(worker.is_alive())
        self.assertTrue(completed_while_locked)
        self.assertTrue(start_available)
        self.assertEqual(outcomes, ["deferred"])


class MobileStateHttpContract(unittest.TestCase):
    lead = fixture.WorkspaceContract.lead
    agent_update = fixture.WorkspaceContract.agent_update

    def setUp(self):
        fixture.WorkspaceContract.setUp(self)
        self.canvas = Canvas(self.state)
        self.canvas.runtime = self.runtime
        self.static = self.root / "web"
        self.static.mkdir()
        self.worker = (Path(__file__).resolve().parents[1]
                       / "web/public/studio-sw.js").read_bytes()
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

    def test_busy_connection_lock_returns_retryable_sync_pull_then_restores_notice(self):
        lead = self.lead()
        with self.runtime.start_lock:
            self.runtime.connection_ids["default"] = "http-stable-connection"
        with self.runtime.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_native_notices(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            self.runtime.put(db, "native_notices", {
                "id": "http-deferred-notice", "accountKey": "default",
                "connectionId": "http-stable-connection", "message": "Visible after retry",
            })

        warm = self.get("/api/sync/pull?scope=state:chat")
        warm_payload = json.loads(warm["documents"][0]["payload"])
        self.assertEqual([notice["id"] for notice in warm_payload["runtime"]["nativeNotices"]],
                         ["http-deferred-notice"])
        self.runtime.start_lock.acquire()
        try:
            # Invalidate the cached state while native connection identity is
            # unavailable. No further database write occurs after the 503.
            self.agent_update(lead, name="Updated during deferred snapshot")
            status, _, body = self.request("/api/sync/pull?scope=state:chat")
        finally:
            self.runtime.start_lock.release()
        self.assertEqual(status, 503, body)
        self.assertIn("retry shortly", json.loads(body)["error"])

        result = self.get("/api/sync/pull?scope=state:chat")
        payload = json.loads(result["documents"][0]["payload"])
        self.assertEqual([notice["id"] for notice in payload["runtime"]["nativeNotices"]],
                         ["http-deferred-notice"])
        self.assertEqual(next(agent for agent in payload["runtime"]["agents"]
                              if agent["id"] == lead["id"])["name"],
                         "Updated during deferred snapshot")

    def assert_no_work_reads(self, path):
        original = self.runtime.records
        reads = []

        def record_reads(db, table):
            reads.append(table)
            return original(db, table)

        with patch.object(self.runtime, "records", side_effect=record_reads):
            with patch.object(self.runtime, "snapshot", wraps=self.runtime.snapshot) as snapshot:
                result = self.get(path)
                self.assertEqual(snapshot.call_count, 1, f"{path} repeated the runtime snapshot")
        self.assertNotIn("work", reads, f"{path} loaded work history")
        return result

    def test_chat_get_and_sync_skip_history_but_full_views_retain_it(self):
        lead = self.lead()
        work = self.runtime.work_action(lead["id"], {
            "action": "create", "title": "Exact evidence",
            "description": "Saved instructions " * 200,
        })
        identity = self.get("/api/sync/identity")
        self.assertTrue(identity["chatState"])
        full = self.get("/api/state")
        chat = self.assert_no_work_reads("/api/state?view=chat")
        self.assertTrue(chat["token"])
        self.assertEqual(chat["token"], full["token"])
        self.assertNotIn("board", chat)
        self.assertNotIn("board", full)
        self.assertNotIn("work", chat["runtime"])
        self.assertEqual(full["runtime"]["work"][0]["id"], work["id"])
        for field in ("threads", "chats", "nodes", "edges", "stateDir"):
            self.assertEqual(chat[field], full[field], field)
        self.assertEqual(chat["runtime"], {
            key: value for key, value in full["runtime"].items() if key != "work"
        })
        for scope in ("state", "state:chat"):
            path = "/api/sync/pull?scope=" + scope
            pull = self.assert_no_work_reads(path) if scope == "state:chat" else self.get(path)
            self.assertEqual(pull["workspaceId"], identity["workspaceId"])
            self.assertEqual(len(pull["documents"]), 1)
            document = pull["documents"][0]
            self.assertEqual(document["id"], scope)
            self.assertFalse(document["_deleted"])
            payload = json.loads(document["payload"])
            self.assertNotIn("token", payload)
            self.assertNotIn(full["token"], document["payload"])
            self.assertNotIn("at", payload)
            self.assertEqual("work" in payload["runtime"], scope == "state")
        details = self.get("/api/work?agent=" + lead["id"])
        self.assertEqual(details["items"][0]["description"], work["description"])

    def test_volatile_rate_and_connection_changes_advance_state_without_broad_writes(self):
        before = self.get("/api/sync/generations")
        with self.runtime.db() as db:
            broad_before = db.execute("SELECT value FROM sync_generation WHERE id=1").fetchone()[0]
        with self.runtime.lock:
            self.runtime.rate_limits_by_account["signature-only"] = {
                "accountKey": "signature-only", "data": {"remaining": 7},
            }
            self.runtime.connection_ids["default"] = "signature-only-replacement"
        after = self.get("/api/sync/generations")
        with self.runtime.db() as db:
            broad_after = db.execute("SELECT value FROM sync_generation WHERE id=1").fetchone()[0]
        self.assertGreater(after["generations"]["state"], before["generations"]["state"])
        self.assertEqual(broad_after, broad_before)

    def test_generation_endpoint_does_not_wait_for_native_start_lock(self):
        self.runtime.start_lock.acquire()
        started = time.monotonic()
        try:
            state = self.get("/api/sync/generations")
            elapsed = time.monotonic() - started
        finally:
            self.runtime.start_lock.release()
        self.assertIn("state", state["generations"])
        self.assertLess(elapsed, 1.0)

    def test_provider_warning_changes_refresh_cached_state_without_database_writes(self):
        from codex_provider_versions import ProviderVersionMonitor
        monitor = self.runtime.provider_version_monitor = ProviderVersionMonitor()
        initial = self.get("/api/sync/pull?scope=state:chat")
        checkpoint = initial["checkpoint"]["seq"]
        with self.runtime.db() as db:
            broad_before = db.execute("SELECT value FROM sync_generation WHERE id=1").fetchone()[0]
        with self.runtime.lock, monitor.lock:
            monitor.providers = [{
                "id": "provider-version:default", "accountKey": "default",
                "provider": "codex", "status": "outdated", "runningVersion": "0.1.0",
                "baseline": "0.153.4", "message": "Synthetic old version", "at": 1,
            }]
        updated = self.get(f"/api/sync/pull?scope=state:chat&after={checkpoint}")
        payload = json.loads(updated["documents"][0]["payload"])
        self.assertEqual(payload["runtime"]["nativeNotices"][0]["message"], "Synthetic old version")
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT value FROM sync_generation WHERE id=1").fetchone()[0], broad_before)
        checkpoint = updated["checkpoint"]["seq"]
        with self.runtime.lock, monitor.lock:
            monitor.providers = []
        cleared = self.get(f"/api/sync/pull?scope=state:chat&after={checkpoint}")
        self.assertEqual(json.loads(cleared["documents"][0]["payload"])["runtime"]["nativeNotices"], [])

    def test_checkpoint_and_workspace_survive_adapter_reload(self):
        lead = self.lead()
        identity = self.get("/api/sync/identity")
        initial = self.get("/api/sync/pull?scope=state:chat")
        checkpoint = initial["checkpoint"]["seq"]
        self.assertGreater(checkpoint, 0)
        # A full-state read receives its own document and cannot advance this scope.
        self.get("/api/sync/pull?scope=state")
        unchanged = self.get(f"/api/sync/pull?scope=state:chat&after={checkpoint}")
        self.assertEqual(unchanged["documents"], [])
        self.assertEqual(unchanged["checkpoint"], {"seq": checkpoint})
        self.stop_server()
        self.start_server()
        self.assertEqual(self.get("/api/sync/identity"), identity)
        self.assertEqual(
            self.get(f"/api/sync/pull?scope=state:chat&after={checkpoint}")["documents"], []
        )
        self.agent_update(lead, name="Updated after reload")
        changed = self.get(f"/api/sync/pull?scope=state:chat&after={checkpoint}")
        self.assertGreater(changed["checkpoint"]["seq"], checkpoint)
        self.assertEqual(changed["workspaceId"], identity["workspaceId"])
        payload = json.loads(changed["documents"][0]["payload"])
        self.assertEqual(payload["runtime"]["agents"][0]["name"], "Updated after reload")
        self.assertNotIn("work", payload["runtime"])

    def test_graph_alias_does_not_change_canonical_runtime_parent(self):
        lead = self.agent_update(self.lead(), threadId="exact-native-thread")
        with self.canvas.connect() as db:
            db.execute("INSERT INTO graph_agents VALUES (?,?)", ("alias", json.dumps({
                "id": "alias", "threadId": lead["threadId"],
                "parentId": "graph-parent", "name": "Graph alias",
            })))
        for path in ("/api/state", "/api/state?view=chat"):
            state = self.get(path)
            canonical = next(agent for agent in state["runtime"]["agents"]
                             if agent["id"] == lead["id"])
            graph = next(agent for agent in state["threads"] if agent["id"] == lead["id"])
            self.assertEqual(canonical["parentId"], lead["parentId"])
            self.assertNotIn("graphAlias", canonical)
            self.assertEqual(graph["parentId"], "graph-parent")
            self.assertEqual(graph["graphAlias"], "alias")

    def test_invalid_scope_never_returns_or_stores_a_state_document(self):
        self.lead()
        self.get("/api/sync/identity")
        for scope in ("state:unknown", "state:chat:other", "state:chat/", "State:chat"):
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
        paths = ("/studio-sw.js", "/api/state?view=chat",
                 "/api/sync/identity", "/api/sync/pull?scope=state:chat")
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
