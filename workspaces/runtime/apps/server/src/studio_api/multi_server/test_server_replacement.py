"""A lost server identity replaced by its successor: two real runtimes and the owner API."""
from __future__ import annotations

import json
from pathlib import Path
import runpy
import sys
import tempfile
from typing import Any, ClassVar, cast
import unittest
from unittest.mock import patch

from fastapi.testclient import TestClient

from codex_remote import RemoteAccess
from codex_server_replacement import previous_local_identities, replace_references
from studio_api.app import create_app
from studio_api.context import ApiContext
from studio_api.multi_server.test_access import FixtureCanvas, FixtureRuntime

ROOT = Path(__file__).resolve().parents[3]
OWNER = "owner@example.com"
ORIGIN = "https://lumina.example.ts.net"


def fixture(name: str) -> dict[str, Any]:
    sys.path.insert(0, str(ROOT / "tests"))
    try:
        return runpy.run_path(str(ROOT / "tests" / name), run_name="server_replacement_fixture")
    finally:
        sys.path.pop(0)


CROSS = fixture("multi-server-orchestration-contract.py")


class ReplaceReferencesTests(unittest.TestCase):
    """The home server loses its identity ("home" becomes "home2"); the remote keeps its links."""

    def setUp(self) -> None:
        self.case = CROSS["CrossServer"]()
        self.case.setUp()
        self.addCleanup(self.case.tearDown)
        self.case.root = self.case.root.resolve()
        self.home, self.remote = self.case.home, self.case.remote
        project = self.home.projects({"path": str(self.case.root / "home"), "name": "Shared"})
        self.project = self.home.projects({"action": "rename", "path": project["id"], "name": "Shared", "expected_revision": 0})
        located = self.case.network["home"].tools(self.case.lead, {
            "action": "add_location", "project": self.project["id"], "server": "remote",
            "path": str(self.case.root / "remote"), "request_id": "location-one"}, "location-one")
        self.assertEqual(located["outcome"], "applied")
        self.worker = self.case.spawn()["agents"][0]["id"]
        # The home state survives, but its server identity is new.
        self.case.network["home2"] = self.case.network.pop("home")
        self.case.transports["home"].local_server_id = "home2"

    def replace(self, runtime: Any, old: str, new: str) -> dict[str, Any]:
        with runtime.lock, runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            return replace_references(runtime, db, old, new)

    def remote_project(self) -> dict[str, Any]:
        return cast(dict[str, Any], next(row for row in self.remote.projects()["items"] if row.get("projectAliases")))

    def test_remote_rewrites_live_references_and_never_resends_in_flight_requests(self) -> None:
        sender = self.case.network["remote"]
        actor = self.remote.agent(self.worker)
        lost = sender.worker_call(actor, "context", {"topic": "plan"}, "read-before")
        self.assertEqual(lost["outcome"], "unknown")
        key = lost["requestId"]
        with self.remote.lock, self.remote.db() as db:
            # A turn needs a slot admission from the home; this request can never be answered.
            self.assertFalse(sender.admission(db, self.remote.agent(self.worker, db)))
        admission = self.remote.agent(self.worker)["remoteAdmission"]["id"]
        with self.remote.read_db() as db:
            pending = [row[0] for row in db.execute(
                "SELECT id FROM runtime_server_outbox WHERE server='home' AND state!='complete' ORDER BY rowid")]
        self.assertIn(key, pending)
        self.assertIn(admission, pending)

        changes = self.replace(self.remote, "home", "home2")

        self.assertEqual(changes["reconcile"], pending)
        # The worker asks the successor for a new admission instead of waiting forever.
        self.assertNotIn("remoteAdmission", self.remote.agent(self.worker))
        self.assertEqual(changes["rewritten"]["projects"], 1)
        self.assertEqual(changes["rewritten"]["links"], 1)
        self.assertGreaterEqual(changes["rewritten"]["agents"], 2)
        project = self.remote_project()
        self.assertEqual(project["projectAliases"], [{"serverId": "home2", "projectId": self.project["id"], "name": "Shared"}])
        with self.remote.read_db() as db:
            entity = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='project' AND id=?",
                                           (project["id"],)).fetchone()[0])
            link = json.loads(db.execute("SELECT record FROM runtime_server_links").fetchone()[0])
            receipt = db.execute("SELECT state,result,error FROM runtime_server_outbox WHERE id=?", (key,)).fetchone()
        self.assertEqual(entity["value"]["projectAliases"][0]["serverId"], "home2")
        self.assertEqual((link["side"], link["home"]), ("remote", "home2"))
        self.assertEqual(self.remote.agent(self.worker)["remoteOrigin"]["home"], "home2")
        self.assertEqual(self.remote.agent(actor["parentId"])["remoteAnchor"]["home"], "home2")
        self.assertEqual((receipt["state"], receipt["error"]), ("complete", "server_replaced"))
        self.assertEqual(json.loads(receipt["result"])["outcome"], "unknown")
        calls = len(self.case.transports["remote"].calls)
        self.assertEqual(sender.deliver(key)["outcome"], "unknown")
        self.assertEqual(len(self.case.transports["remote"].calls), calls)

        # The successor receives new requests through the same link.
        result = sender.worker_call(self.remote.agent(self.worker), "context", {"topic": "plan"}, "read-after")
        self.assertEqual(result["topic"], "plan")
        # A repeated replacement finds nothing left to change.
        self.assertEqual(self.replace(self.remote, "home", "home2"),
                         {"rewritten": {"projects": 0, "agents": 0, "links": 0, "moves": 0, "commands": 0}, "reconcile": []})

    def test_home_records_its_own_previous_identity_once(self) -> None:
        with self.home.read_db() as db:
            self.assertEqual(previous_local_identities(db, "home2"), {"home"})
        changes = self.replace(self.home, "home", "home2")
        self.assertEqual(changes["rewritten"]["links"], 1)
        self.assertEqual(changes["rewritten"]["projects"], 1)
        project = next(row for row in self.home.projects()["items"] if row["id"] == self.project["id"])
        self.assertEqual(project["homeServerId"], "home2")
        self.assertEqual({row["serverId"] for row in project["locations"]}, {"home2", "remote"})
        with self.home.read_db() as db:
            self.assertEqual(previous_local_identities(db, "home2"), set())
        # The remote link refuses the successor until the remote records the replacement too.
        receiver = self.case.network["remote"]
        with self.remote.read_db() as db:
            link = db.execute("SELECT id FROM runtime_server_links").fetchone()[0]
        rebind = {"action": "rebind", "payload": {"link": link, "parentEpoch": 0}}
        refused = receiver.receive("home2", {**rebind, "requestId": "rebind-before"})
        self.assertEqual(refused["outcome"], "not_applied")
        self.assertIn("another server", refused["error"])
        self.replace(self.remote, "home", "home2")
        self.assertEqual(receiver.receive("home2", {**rebind, "requestId": "rebind-after"})["outcome"], "applied")
        lead = self.case.lead
        self.home.send(self.worker, "Continue", "input-after", manual=False, resume=True,
                       sender=lead["id"], sender_epoch=lead["epoch"])
        with self.home.read_db() as db:
            queued = [row[0] for row in db.execute("SELECT id FROM runtime_server_outbox WHERE state='queued' ORDER BY rowid")]
        for queued_key in queued:
            self.case.network["home2"].deliver(queued_key)
        with self.remote.read_db() as db:
            rows = db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND text='Continue'", (self.worker,)).fetchone()
        self.assertEqual(rows[0], 1)

    def test_owner_operation_rewrites_runtime_state_in_one_transaction(self) -> None:
        service = self.remote.paired_access()
        discovery = service.discovery()
        with self.remote.db() as db:
            for server_id, table, status in (("home2", "runtime_access_clients", "paired"),
                                             ("home", "runtime_access_discovered", "unreachable")):
                db.execute(f"INSERT INTO {table} VALUES(?,?)", (server_id, json.dumps({
                    "id": server_id, "clientId": server_id, "serverId": server_id, "kind": "server",
                    "label": "Home", "origin": ORIGIN, "publicKey": server_id + "-key", "tailscaleUser": OWNER,
                    "status": status, "created": 1, "lastAccess": None, "revoked": None})))
        presented = {"protocol": 1, "serverId": "home2", "publicKey": "home2-key", "origin": ORIGIN,
                     "label": "Home", "tailscaleUser": OWNER, "autoPair": True}
        with patch("codex_multi_server._owner_login", return_value=OWNER), \
                patch.object(discovery, "probe", return_value=presented):
            alerts = service.snapshot()["identityAlerts"]
            self.assertIn(("peer_identity_changed", "home", "home2"),
                          [(row["code"], row["oldServerId"], row["newServerId"]) for row in alerts])
            state = service.replace_server("home", "home2", "replace-home", "local")
        self.assertNotIn("home", [row["oldServerId"] for row in state["identityAlerts"]])
        replaced = next(row for row in state["clients"] if row["id"] == "home")
        self.assertEqual((replaced["status"], replaced["replacedBy"]), ("revoked", "home2"))
        self.assertEqual(self.remote_project()["projectAliases"][0]["serverId"], "home2")
        self.assertEqual(self.remote.agent(self.worker)["remoteOrigin"]["home"], "home2")


class ReplaceServerApiTests(unittest.TestCase):
    """The owner operation: preconditions, live proof, exact retry, and audit."""

    runtime: ClassVar[FixtureRuntime]
    client: ClassVar[TestClient]

    @classmethod
    def setUpClass(cls) -> None:
        # One application per class: building the FastAPI routes dominates the run time.
        directory = tempfile.TemporaryDirectory(prefix="studio-replace-test-")
        cls.addClassCleanup(directory.cleanup)
        cls.runtime = FixtureRuntime(Path(directory.name))
        cls.addClassCleanup(cls.runtime.service.close)
        context = ApiContext(cast(Any, FixtureCanvas(cls.runtime)), token="local-token",
                             remote=RemoteAccess(cls.runtime.root, "https://studio.example.ts.net"), server_port=8765)
        context.api_schema_hash = "replace-schema"
        cls.client = TestClient(create_app(context), base_url="http://127.0.0.1:8765", client=("127.0.0.1", 9999))
        cls.addClassCleanup(cls.client.close)

    def setUp(self) -> None:
        for target in ("codex_multi_server._owner_login", "codex_multi_server._peer_login"):
            patcher = patch(target, return_value=OWNER)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.probe = patch.object(self.runtime.service.discovery(), "probe", return_value={
            "protocol": 1, "serverId": "new-mac", "publicKey": "new-key", "origin": ORIGIN,
            "label": "Studio server", "tailscaleUser": OWNER, "autoPair": True})
        self.probed = self.probe.start()
        self.addCleanup(self.probe.stop)
        with self.runtime.db() as db:
            for table in ("runtime_access_clients", "runtime_access_discovered", "runtime_access_audit"):
                db.execute(f"DELETE FROM {table}")
            db.execute("INSERT INTO runtime_access_clients VALUES(?,?)", ("new-mac", json.dumps({
                "id": "new-mac", "clientId": "new-mac", "serverId": "new-mac", "kind": "server", "label": "Studio server",
                "origin": ORIGIN, "publicKey": "new-key", "tailscaleUser": OWNER, "status": "paired", "created": 2,
                "lastAccess": None, "revoked": None, "alias": "STU"})))
            db.execute("INSERT INTO runtime_access_discovered VALUES(?,?)", ("old-mac", json.dumps({
                "id": "old-mac", "clientId": "old-mac", "serverId": "old-mac", "kind": "server", "label": "Lumina Mac",
                "origin": ORIGIN, "publicKey": "old-key", "tailscaleUser": OWNER, "status": "unreachable", "created": 1,
                "lastAccess": None, "revoked": None, "lastSeen": 1, "autoPair": True})))

    def post(self, body: dict[str, Any], **headers: str) -> Any:
        return self.client.post("/api/multi-server", json={"action": "replace_server", **body},
                                headers={"X-Canvas-Token": "local-token", **headers})

    def test_replacement_is_recorded_once_and_clears_the_alert(self) -> None:
        before = self.client.get("/api/multi-server", headers={"X-Canvas-Token": "local-token"}).json()
        self.assertEqual([(row["code"], row["oldServerId"], row["newServerId"]) for row in before["identityAlerts"]],
                         [("peer_identity_changed", "old-mac", "new-mac")])
        body = {"oldServerId": "old-mac", "newServerId": "new-mac", "requestId": "replace-1"}
        first = self.post(body)
        self.assertEqual(first.status_code, 200, first.text)
        state = first.json()
        self.assertEqual(state["identityAlerts"], [])
        old = next(row for row in state["clients"] if row["id"] == "old-mac")
        self.assertEqual((old["status"], old["replacedBy"], old["label"]), ("revoked", "new-mac", "Lumina Mac"))
        self.assertNotIn("old-mac", [row["id"] for row in state["servers"] if row["status"] != "revoked"])
        self.assertEqual(self.post(body).status_code, 200)
        self.assertEqual(self.post({**body, "requestId": "replace-2"}).status_code, 200)
        self.assertEqual(self.post({**body, "newServerId": "other"}).json()["code"], "request_conflict")
        self.assertEqual(self.probed.call_count, 1)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_audit WHERE action='replace_server'").fetchone()[0], 1)
            self.assertIsNone(db.execute("SELECT 1 FROM runtime_access_discovered WHERE id='old-mac'").fetchone())
        audit = self.client.get("/api/multi-server/audit", headers={"X-Canvas-Token": "local-token"}).json()
        self.assertEqual((audit["records"][0]["action"], audit["records"][0]["clientId"]), ("replace_server", "old-mac"))

    def test_refusals_change_nothing(self) -> None:
        body = {"oldServerId": "old-mac", "newServerId": "new-mac", "requestId": "replace-refused"}
        proxy = {"X-Forwarded-For": "100.64.0.12", "X-Forwarded-Host": "studio.example.ts.net", "X-Forwarded-Proto": "https"}
        self.assertEqual(self.post(body, **proxy).status_code, 403)
        self.probed.return_value = {**self.probed.return_value, "serverId": "old-mac", "publicKey": "old-key"}
        self.assertEqual(self.post(body).json()["code"], "identity_mismatch")
        self.probed.return_value = {**self.probed.return_value, "serverId": "new-mac", "publicKey": "new-key"}
        cases = [({"newServerId": "missing"}, 403, "server_not_paired"),
                 ({"oldServerId": "never-seen"}, 409, "unknown_server"),
                 ({"oldServerId": self.runtime.service.local_server_id}, 400, "invalid_replacement"),
                 ({"oldServerId": "new-mac"}, 400, "invalid_replacement")]
        for change, status, code in cases:
            with self.subTest(change=change):
                reply = self.post({**body, **change})
                self.assertEqual((reply.status_code, reply.json()["code"]), (status, code), reply.text)
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_access_discovered SET record=json_set(record,'$.origin','https://other.example.ts.net')")
        self.assertEqual(self.post(body).json()["code"], "identity_mismatch")
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_audit WHERE action='replace_server'").fetchone()[0], 0)
            self.assertIsNone(db.execute("SELECT 1 FROM runtime_access_clients WHERE id='old-mac'").fetchone())


if __name__ == "__main__":
    unittest.main()
