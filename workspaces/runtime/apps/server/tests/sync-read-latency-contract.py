#!/usr/bin/env python3
"""Independent entity pulls do not wait in a transcript or writer queue."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import http.client
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from urllib.parse import urlencode
import zlib
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_canvas import Canvas, make_server
import codex_sync
from codex_sync import SCOPE_STRIPES
from codex_sync_entities import install_bypass_triggers, register_functions


BUSY_TIMEOUT = .2
CLIENT_DEADLINE = .3
ENTITY_SCOPE = "state:entities:v1"


class SyncReadLatencyContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-sync-read-latency-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.path = self.root / "canvas.sqlite3"
        self.agent = {"id": "owner", "rootId": "owner", "name": "Owner", "kind": "agent",
                      "status": "paused", "deletedAt": None}
        self.monitor = {"id": "monitor", "agent": "owner", "status": "running",
                        "created": 1, "tail": "First output"}
        self.transcript_entered = threading.Event()
        self.transcript_release = threading.Event()
        self.addCleanup(self.transcript_release.set)
        self.blocked_agent = None
        self.observe_writer = False
        self.first_writer_attempt = threading.Event()
        self.observation_lock = threading.Lock()
        self.writer_attempts = []
        self.pull_observations = []
        self.two_pulls_finished = threading.Event()

        self.canvas = Canvas(self.root)
        self.canvas.connect = self.connect
        self.canvas.transcript = self.transcript
        with self.connect() as db:
            db.executescript("""PRAGMA journal_mode=WAL;
                CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT);
                CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,status TEXT,
                                            created REAL,error TEXT);""")
            db.execute("INSERT INTO runtime_agents VALUES (?,?)", ("owner", json.dumps(self.agent)))
            db.execute("INSERT INTO runtime_monitors VALUES (?,?)", ("monitor", json.dumps(self.monitor)))
            install_bypass_triggers(db)

        self.server = make_server(self.canvas)
        self.addCleanup(self.server.server_close)
        self.store = self.server._context.sync()
        # Finish schema creation and the first seed before either contention case.
        self.initial = self.store.pull(ENTITY_SCOPE, fresh=True)
        self.identity = self.store.identity()
        original_pull = self.store.pull

        def observed_pull(*args, **kwargs):
            row = {"started": time.monotonic(), "scope": args[0]}
            with self.observation_lock:
                self.pull_observations.append(row)
            try:
                return original_pull(*args, **kwargs)
            finally:
                with self.observation_lock:
                    row["finished"] = time.monotonic()
                    if sum("finished" in item for item in self.pull_observations) >= 2:
                        self.two_pulls_finished.set()

        self.store.pull = observed_pull
        self.server_thread = threading.Thread(target=self.server.serve_forever,
                                              kwargs={"poll_interval": .01}, daemon=True)
        self.server_thread.start()
        self.addCleanup(self.stop_server)
        self.pool = ThreadPoolExecutor(max_workers=3)
        self.addCleanup(self.pool.shutdown, wait=True)

    def stop_server(self):
        self.transcript_release.set()
        self.server.shutdown()
        self.server_thread.join(timeout=1)

    @contextmanager
    def connect(self):
        db = sqlite3.connect(self.path, timeout=BUSY_TIMEOUT)
        db.row_factory = sqlite3.Row
        register_functions(db)

        def trace(statement):
            if self.observe_writer and statement.strip().upper() == "BEGIN IMMEDIATE":
                with self.observation_lock:
                    self.writer_attempts.append(time.monotonic())
                self.first_writer_attempt.set()

        db.set_trace_callback(trace)
        try:
            with db:
                yield db
        finally:
            db.close()

    @contextmanager
    def writer(self):
        db = sqlite3.connect(self.path)
        self.assertEqual(db.execute("PRAGMA journal_mode").fetchone()[0], "wal")
        db.execute("BEGIN IMMEDIATE")
        self.assertTrue(db.in_transaction)
        try:
            yield db
        finally:
            db.rollback()
            db.close()

    def transcript(self, agent):
        if agent == self.blocked_agent:
            self.transcript_entered.set()
            if not self.transcript_release.wait(2):
                raise RuntimeError("The private transcript fixture did not release")
        return {"id": agent, "items": []}

    def get(self, path, timeout=CLIENT_DEADLINE):
        started = time.monotonic()
        connection = http.client.HTTPConnection("127.0.0.1", self.server.server_port, timeout=timeout)
        try:
            connection.request("GET", path)
            response = connection.getresponse()
            return {"status": response.status, "body": json.loads(response.read()),
                    "elapsed": time.monotonic() - started}
        except TimeoutError:
            return {"timeout": True, "elapsed": time.monotonic() - started}
        finally:
            connection.close()

    def pull_path(self, scope=ENTITY_SCOPE, **query):
        return "/api/sync/pull?" + urlencode({"scope": scope, **query})

    def assert_controls_fast(self):
        for path in ("/api/session", "/api/sync/identity"):
            response = self.get(path, timeout=.1)
            self.assertEqual(response.get("status"), 200, (path, response))
            self.assertLess(response["elapsed"], .1, (path, response))
            if path.endswith("identity"):
                self.assertEqual(response["body"], self.identity)
            else:
                self.assertTrue(response["body"].get("token"))

    def test_identity_reuses_committed_reader_without_canvas_connection_setup(self):
        original_connect = self.store.connect
        connections = []

        @contextmanager
        def counted_connect(site="SyncStore"):
            connections.append(site)
            with original_connect(site) as db:
                yield db

        self.store.connect = counted_connect
        with self.connect() as db:
            db.execute("UPDATE sync_identity SET id='committed-workspace-id'")

        responses = [self.pool.submit(self.get, "/api/sync/identity") for _ in range(4)]
        responses = [response.result(timeout=1) for response in responses]

        for response in responses:
            self.assertEqual(response["status"], 200, response)
            self.assertEqual(response["body"]["workspaceId"], "committed-workspace-id")
        self.assertEqual(connections, [])

    def test_unchanged_entity_pull_checks_only_the_bounded_monitor_projection(self):
        first = self.store.pull(ENTITY_SCOPE, fresh=True)
        cursor = first["checkpoint"]["seq"]
        stabilized = self.store.pull(ENTITY_SCOPE, cursor)
        self.assertEqual(stabilized["documents"], [])

        with patch.object(codex_sync.json, "dumps", wraps=json.dumps) as dumps:
            unchanged = self.store.pull(ENTITY_SCOPE, cursor)
        self.assertEqual(unchanged["documents"], [])
        self.assertEqual(unchanged["checkpoint"]["seq"], cursor)
        self.assertEqual(len(dumps.call_args_list), 1)
        self.assertEqual(dumps.call_args.args[0]["collection"], "monitor")

        with self.connect() as db:
            db.execute("UPDATE runtime_monitors SET record=? WHERE id=?",
                       (json.dumps({**self.monitor, "status": "completed"}), "monitor"))
        with patch.object(codex_sync.json, "dumps", wraps=json.dumps) as dumps:
            changed = self.store.pull(ENTITY_SCOPE, cursor)
        self.assertTrue(dumps.call_args_list)
        self.assertTrue(all(call.args[0].get("collection") == "monitor"
                            for call in dumps.call_args_list))
        self.assertEqual(len(changed["documents"]), 1)
        entity = json.loads(changed["documents"][0]["payload"])
        self.assertEqual(entity["collection"], "monitor")
        self.assertEqual(entity["value"]["status"], "completed")
        self.assertGreater(changed["checkpoint"]["seq"], cursor)

    def test_writer_conflicts_do_not_serialize_two_entity_http_deadlines(self):
        changed = {**self.monitor, "tail": "The final output", "status": "completed"}
        with self.connect() as db:
            db.execute("UPDATE runtime_monitors SET record=? WHERE id=?", (json.dumps(changed), "monitor"))
        path = self.pull_path(after=self.initial["checkpoint"]["seq"])
        self.observe_writer = True
        with self.writer():
            first = self.pool.submit(self.get, path)
            self.assertTrue(self.first_writer_attempt.wait(.5), "The first pull did not request the SQLite writer")
            second = self.pool.submit(self.get, path)
            self.assert_controls_fast()
            responses = [first.result(timeout=1), second.result(timeout=1)]
            # Keep the writer held through both real handlers, including a handler
            # whose client already timed out. A timed out GET does not stop Python.
            self.assertTrue(self.two_pulls_finished.wait(1), self.pull_observations)
        self.observe_writer = False

        evidence = {"responses": responses, "handlerSeconds": [
            round(row["finished"] - row["started"], 4) for row in self.pull_observations]}
        for response in responses:
            self.assertNotIn("timeout", response, evidence)
            self.assertLess(response["elapsed"], CLIENT_DEADLINE, evidence)
            # Contention must stay visible. It must not acknowledge a cursor from
            # an entity projection whose committed source still needs maintenance.
            self.assertIn(response["status"], (400, 503), evidence)
            self.assertIn("locked", response["body"].get("error", "").lower(), evidence)
            self.assertNotIn("checkpoint", response["body"], evidence)

        # Two ordinary callers repair the same source after the writer releases.
        # Both must expose the exact value and the same durable sequence.
        repaired = [future.result(timeout=1) for future in
                    [self.pool.submit(self.get, path), self.pool.submit(self.get, path)]]
        for response in repaired:
            self.assertEqual(response["status"], 200, response)
            body = response["body"]
            row = next(row for row in body["documents"] if row["id"] == "entity:monitor:monitor")
            value = json.loads(row["payload"])["value"]
            self.assertEqual((value["tail"], value["status"]), (changed["tail"], changed["status"]))
            self.assertEqual(body["checkpoint"]["seq"], body["maxSeq"])
            self.assertGreater(body["checkpoint"]["seq"], self.initial["checkpoint"]["seq"])
        self.assertEqual(repaired[0]["body"], repaired[1]["body"])

    def test_crc32_transcript_collision_does_not_block_an_unchanged_entity_pull(self):
        stripe = zlib.crc32(ENTITY_SCOPE.encode()) % SCOPE_STRIPES
        self.blocked_agent = next("collision-" + str(index) for index in range(4096)
                                  if zlib.crc32(("transcript:collision-" + str(index)).encode())
                                  % SCOPE_STRIPES == stripe)
        scope = "transcript:" + self.blocked_agent
        self.assertNotEqual(scope, ENTITY_SCOPE)
        self.assertEqual(zlib.crc32(scope.encode()) % SCOPE_STRIPES, stripe)
        transcript = self.pool.submit(self.get, self.pull_path(scope), 1)
        try:
            self.assertTrue(self.transcript_entered.wait(.5), "The transcript callback did not enter")
            self.assert_controls_fast()
            response = self.get(self.pull_path(fresh=1))
            self.assertFalse(self.transcript_release.is_set())
        finally:
            self.transcript_release.set()
            transcript_response = transcript.result(timeout=1)

        self.assertEqual(transcript_response.get("status"), 200, transcript_response)
        self.assertNotIn("timeout", response, {"stripe": stripe, "scope": scope, "response": response})
        self.assertLess(response["elapsed"], CLIENT_DEADLINE, response)
        self.assertEqual(response["status"], 200, response)
        body = response["body"]
        self.assertEqual(body["documents"], self.initial["documents"])
        self.assertEqual(body["checkpoint"], self.initial["checkpoint"])
        self.assertEqual(body["maxSeq"], self.initial["maxSeq"])
        self.assertEqual(body["workspaceId"], self.identity["workspaceId"])


if __name__ == "__main__":
    unittest.main()
