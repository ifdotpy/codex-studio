#!/usr/bin/env python3
"""A rule tick shares unchanged owners only within its current SQL phase."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
from contextlib import contextmanager
import json
import os
from pathlib import Path
import sqlite3
import statistics
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_rules import RulesMixin
from codex_runtime import Runtime

if os.environ.get("CODEX_RULES_TICK_SOURCE"):
    tree = ast.parse(Path(os.environ["CODEX_RULES_TICK_SOURCE"]).read_text())
    owner = next(node for node in tree.body if isinstance(node, ast.ClassDef)
                 and node.name == "RulesMixin")
    node = next(node for node in owner.body if isinstance(node, ast.FunctionDef)
                and node.name == "rules_tick")
    namespace = {}
    exec(compile(ast.Module(body=[node], type_ignores=[]), "<frozen-rule-tick>", "exec"),
         RulesMixin.rules_tick.__globals__, namespace)
    RulesMixin.rules_tick = namespace["rules_tick"]


class CapturePool:
    def __init__(self):
        self.launched = []

    def submit(self, callback, rule):
        self.launched.append(rule)


class RuleFixture(RulesMixin):
    """Real owner decoding and enqueue with private SQLite, no backend threads."""
    enqueue = Runtime.enqueue
    _recovery_event_pending = Runtime._recovery_event_pending

    def __init__(self, directory):
        self.path = Path(directory) / "rules.sqlite3"
        self.lock = threading.RLock()
        self.closed = False
        self.changed = threading.Event()
        self.pool = CapturePool()
        self._agent_record_revision = 0
        self.owner_loads = []
        self.after_agent_load = None
        self.after_enqueue = None
        self.phase = None
        with self.db() as db:
            db.executescript("""
                CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,
                    status TEXT,created REAL,epoch INTEGER,error TEXT,delivered REAL);
            """)

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path)
        db.row_factory = sqlite3.Row
        previous, self.phase = self.phase, "writer"
        try:
            with db:
                yield db
        finally:
            self.phase = previous
            db.close()

    @contextmanager
    def read_db(self):
        db = sqlite3.connect(self.path)
        db.row_factory = sqlite3.Row
        db.execute("PRAGMA query_only=ON")
        previous, self.phase = self.phase, "read"
        try:
            yield db
        finally:
            self.phase = previous
            db.close()

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(
            f"SELECT record FROM runtime_{table} ORDER BY rowid")]

    def agent(self, key, db):
        actor = Runtime.agent(self, key, db)
        self.owner_loads.append((self.phase, key, actor.copy()))
        if self.after_agent_load:
            self.after_agent_load(self, db, actor)
        return actor

    def put(self, db, table, record):
        db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES(?,?)",
                   (record["id"], json.dumps(record)))
        if table == "agents":
            self._agent_record_revision += 1

    def team_agents(self, db, key):
        return []

    def schedule(self):
        raise AssertionError("The fixture must not start the scheduler")

    def _stage_event_resources(self, db, key):
        pass

    def mark_event_timing(self, db, keys, stage):
        pass

    def mark_event_timings(self, db, keys, fields):
        pass

    def enqueue_recovery_event(self, db, actor, kind, text, key):
        result = Runtime.enqueue_recovery_event(self, db, actor, kind, text, key)
        if self.after_enqueue:
            self.after_enqueue(self, db, actor)
        return result

    def add_owner(self, key="owner", **fields):
        actor = {"id": key, "isLead": True, "epoch": 1, "accountKey": "fixture-account",
                 "threadId": "fixture-thread", "autoWake": True, "status": "completed",
                 "name": key, "role": "orchestrator", "approvalPolicy": "never",
                 "sandbox": "workspace-write", "profile": None}
        actor.update(fields)
        with self.db() as db:
            self.put(db, "agents", actor)
        return actor

    def add_rule(self, key, owner="owner", **fields):
        rule = {"id": key, "agent": owner, "epoch": 1, "status": "active", "kind": "interval",
                "inFlight": False, "name": key, "text": "fixture rule", "command": "",
                "created": time.time(), "checks": 0, "wakes": 0,
                "nextAt": time.time() + 3600, "intervalSeconds": 60}
        rule.update(fields)
        with self.db() as db:
            self.put(db, "rules", rule)
        return rule

    def load(self, table, key):
        with self.read_db() as db:
            row = db.execute(f"SELECT record FROM runtime_{table} WHERE id=?", (key,)).fetchone()
            return json.loads(row[0]) if row else None


class OwnerCacheContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="studio-rule-owner-cache-")
        self.runtime = RuleFixture(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def file_rule(self, key, *, stalled=False):
        path = Path(self.tmp.name) / (key + ".txt")
        path.write_text("fixture")
        fingerprint = RulesMixin.file_fingerprint(path)
        return self.runtime.add_rule(key, kind="file", path=str(path), fingerprint=fingerprint,
            nextAt=time.time() - 1, stallTimeoutSeconds=10 if stalled else 0,
            fileActivityAt=time.time() - 100, fileGeneration=1, stallWakeGeneration=-1)

    def test_future_rules_do_not_load_the_owner(self):
        actor = self.runtime.add_owner(history="x" * 65536)
        for index in range(48):
            self.runtime.add_rule(str(index))
        with self.runtime.read_db() as db:
            before = self.runtime.records(db, "rules")
        self.runtime.rules_tick()
        self.assertEqual(self.runtime.owner_loads, [])
        with self.runtime.read_db() as db:
            self.assertEqual(self.runtime.records(db, "rules"), before)
        self.assertEqual(self.runtime.load("agents", "owner"), actor)
        self.assertEqual(self.runtime.pool.launched, [])

    def test_distinct_owners_and_later_ticks_read_each_owner_again(self):
        for key in ("first", "second"):
            self.runtime.add_owner(key)
            for index in range(8):
                self.runtime.add_rule(key + str(index), key, kind="low_workers", minimumWorkers=1,
                                      durationMinutes=1, lowSince=time.time() - 10, alerted=False, activeWorkers=0)
        self.runtime.rules_tick()
        self.assertEqual(len(self.runtime.owner_loads), 4)
        self.runtime.rules_tick()
        self.assertEqual(len(self.runtime.owner_loads), 8)
        self.assertNotIn("_rules_owner_cache", self.runtime.__dict__)

    def test_owner_permission_change_during_stat_rejects_both_old_observations(self):
        self.runtime.add_owner()
        self.file_rule("first")
        self.file_rule("second")
        calls = []
        def fingerprint(path):
            calls.append(path)
            if len(calls) == 1:
                with self.runtime.lock, self.runtime.db() as db:
                    actor = Runtime.agent(self.runtime, "owner", db)
                    actor["approvalPolicy"] = "on-request"
                    self.runtime.put(db, "agents", actor)
            return [1, 2, 3]
        self.runtime.file_fingerprint = fingerprint
        self.runtime.rules_tick()
        self.assertEqual(len(calls), 2)
        self.assertEqual(self.runtime.load("rules", "first")["checks"], 0)
        self.assertEqual(self.runtime.load("rules", "second")["checks"], 0)
        self.assertEqual(self.runtime.pool.launched, [])
        self.assertEqual(len(self.runtime.owner_loads), 2)

    def test_fresh_actor_fields_survive_an_enqueue_after_stat(self):
        self.runtime.add_owner()
        self.file_rule("first", stalled=True)
        def fingerprint(path):
            with self.runtime.lock, self.runtime.db() as db:
                actor = Runtime.agent(self.runtime, "owner", db)
                actor.update(name="Changed during stat", freshField={"value": 7})
                self.runtime.put(db, "agents", actor)
            return RulesMixin.file_fingerprint(path)
        self.runtime.file_fingerprint = fingerprint
        self.runtime.rules_tick()
        actor = self.runtime.load("agents", "owner")
        self.assertEqual(actor["name"], "Changed during stat")
        self.assertEqual(actor["freshField"], {"value": 7})
        self.assertEqual(actor["status"], "queued")
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT id FROM runtime_events").fetchall()[0][0],
                             "rule-stall:first:1")

    def test_rule_finished_and_raw_sql_write_invalidate_the_next_owner(self):
        self.runtime.add_owner(status="running", inFlight=True, turnId="fixture-native-turn")
        self.runtime.add_rule("low", kind="low_workers", minimumWorkers=1, durationMinutes=1,
                              lowSince=time.time() - 100, alerted=False)
        self.file_rule("stall", stalled=True)
        def after_enqueue(runtime, db, actor):
            if db.execute("SELECT COUNT(*) FROM runtime_events").fetchone()[0] == 1:
                revision = runtime._agent_record_revision
                db.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','fresh name',"
                           "'$.status','completed')"
                           " WHERE id='owner'")
                self.assertEqual(runtime._agent_record_revision, revision)
        self.runtime.after_enqueue = after_enqueue
        self.runtime.rules_tick()
        actor = self.runtime.load("agents", "owner")
        self.assertEqual(actor["name"], "fresh name")
        self.assertEqual(actor["status"], "queued")
        self.assertTrue(self.runtime.load("rules", "low")["alerted"])
        self.assertEqual(self.runtime.load("rules", "low")["wakes"], 1)
        self.assertEqual(self.runtime.load("rules", "stall")["stallWakeGeneration"], 1)
        with self.runtime.read_db() as db:
            events = db.execute("SELECT id,status FROM runtime_events ORDER BY id").fetchall()
        self.assertEqual([tuple(row) for row in events],
                         [("rule-stall:stall:1", "pending"), ("rule-wake:low:1", "pending")])
        writer_actors = [actor for phase, _, actor in self.runtime.owner_loads if phase == "writer"]
        self.assertEqual(writer_actors[-1]["name"], "fresh name")
        self.assertEqual(writer_actors[-1]["status"], "completed")

    def test_rule_only_sql_writes_invalidate_without_an_agent_revision(self):
        self.runtime.add_owner()
        for index in range(3):
            self.runtime.add_rule(str(index), nextAt=time.time() - 1)
        revision = self.runtime._agent_record_revision
        self.runtime.rules_tick()
        self.assertEqual(self.runtime._agent_record_revision, revision)
        writer_loads = [item for item in self.runtime.owner_loads if item[0] == "writer"]
        self.assertEqual(len(writer_loads), 3)
        self.assertEqual(len(self.runtime.pool.launched), 3)
        self.assertTrue(all(self.runtime.load("rules", str(index))["inFlight"] for index in range(3)))

    def test_read_phase_agent_revision_invalidates_the_owner(self):
        self.runtime.add_owner()
        for index in range(3):
            self.runtime.add_rule(str(index), nextAt=time.time() - 1)
        changed = []
        def after_load(runtime, db, actor):
            if runtime.phase == "read" and not changed:
                before = db.total_changes
                with runtime.lock, runtime.db() as writer:
                    current = Runtime.agent(runtime, "owner", writer)
                    current["autoWake"] = False
                    runtime.put(writer, "agents", current)
                changed.append(True)
                self.assertEqual(db.total_changes, before)
        self.runtime.after_agent_load = after_load
        self.runtime.rules_tick()
        self.assertEqual(self.runtime.load("rules", "0")["status"], "active")
        self.assertEqual(self.runtime.load("rules", "1")["status"], "paused")
        self.assertEqual(self.runtime.load("rules", "2")["status"], "paused")
        read_actors = [actor for phase, _, actor in self.runtime.owner_loads if phase == "read"]
        self.assertEqual([actor["autoWake"] for actor in read_actors], [True, False])

    def test_exact_recovery_still_blocks_every_rule(self):
        self.runtime.add_owner(restartRecovery={"stage": "pending", "autoWake": True,
            "epoch": 1, "accountKey": "fixture-account", "threadId": "fixture-thread"})
        self.file_rule("first", stalled=True)
        self.file_rule("second", stalled=True)
        self.runtime.file_fingerprint = lambda path: self.fail("Recovery must prevent file reads")
        self.runtime.rules_tick()
        self.assertEqual(self.runtime.pool.launched, [])
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events").fetchone()[0], 0)
        self.assertEqual(len(self.runtime.owner_loads), 2)

    def test_missing_owner_preserves_the_error(self):
        self.runtime.add_rule("missing", "not-present")
        with self.assertRaisesRegex(ValueError, "^Unknown managed agent$"):
            self.runtime.rules_tick()
        self.assertEqual(self.runtime.load("rules", "missing")["status"], "active")


def benchmark():
    with tempfile.TemporaryDirectory(prefix="studio-rule-owner-benchmark-") as directory:
        runtime = RuleFixture(directory)
        runtime.add_owner(history="x" * 131072)
        for index in range(128):
            runtime.add_rule(str(index))
        elapsed, loads = [], []
        for _ in range(7):
            runtime.owner_loads.clear()
            started = time.process_time()
            runtime.rules_tick()
            elapsed.append((time.process_time() - started) * 1000)
            loads.append(len(runtime.owner_loads))
        print(json.dumps({"rules": 128, "ownerBytes": 131072, "runs": 7,
                          "ownerLoadsPerTick": loads,
                          "medianCpuMs": round(statistics.median(elapsed), 3)}))


if __name__ == "__main__":
    if sys.argv[1:] == ["--benchmark"]:
        benchmark()
    else:
        unittest.main()
