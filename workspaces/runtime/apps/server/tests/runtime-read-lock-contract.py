#!/usr/bin/env python3
"""Read views keep a WAL snapshot and do not hold the runtime lock."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import importlib.util
import json
from contextlib import closing
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from studio_api.testing import read_runtime_state
from codex_canvas import Canvas
from codex_native_sweep import _account_busy
from codex_runtime import Runtime
from codex_sync import SyncStore
from studio_api.context import ApiContext

spec = importlib.util.spec_from_file_location(
    "read_contract_server", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class RuntimeReadLock(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, fixture.FakeServer)
        self.runtime.closed = True
        self.runtime.changed.set()
        self.runtime.scheduler.join(timeout=5)
        self.runtime.closed = False
        self.canvas = Canvas(self.root)
        self.canvas.runtime = self.runtime
        self.lead = self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": ""},
                                        draft=True, defer=True)
        self.room = "broadcast:" + self.lead["id"]
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "rooms", {"id": self.room, "kind": "broadcast",
                                           "rootId": self.lead["id"], "updated": time.time()})
            db.execute("INSERT INTO runtime_chat_messages "
                       "(id,room,sender,text,created,deliveries) VALUES (?,?,?,?,?,?)",
                       ("fixture-message", self.room, self.lead["id"], "hello", time.time(), "{}"))
        SyncStore(self.runtime.db, lambda _key: {})

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def test_agent_cache_uses_generation_and_copies_mutable_records(self):
        with self.runtime.read_db() as db:
            first = self.runtime.records(db, "agents", shared=True)
            copied = self.runtime.records(db, "agents")
            self.assertEqual(Runtime.records(db, "agents")[0]["id"], self.lead["id"])
        with self.runtime.read_db() as db:
            self.assertIs(first, self.runtime.records(db, "agents", shared=True))
        copied[0]["name"] = "local change"
        self.assertEqual(first[0]["name"], "Lead")
        with self.runtime.db() as db:
            agent = self.runtime.agent(self.lead["id"], db)
            agent["name"] = "Committed change"
            self.runtime.put(db, "agents", agent)
        with self.runtime.read_db() as db:
            second = self.runtime.records(db, "agents", shared=True)
            self.assertIsNot(first, second)
            self.assertEqual(second[0]["name"], "Committed change")
        with self.assertRaises(RuntimeError):
            with self.runtime.db() as db:
                agent = self.runtime.agent(self.lead["id"], db)
                agent["name"] = "Rolled back"
                self.runtime.put(db, "agents", agent)
                self.runtime.records(db, "agents", shared=True)
                raise RuntimeError("rollback")
        with self.runtime.read_db() as db:
            self.assertIs(second, self.runtime.records(db, "agents", shared=True))

    def test_http_and_chat_reads_complete_while_runtime_lock_is_held(self):
        context = ApiContext(self.canvas)
        entered = threading.Event()
        release = threading.Event()
        def hold():
            with self.runtime.lock:
                entered.set()
                release.wait(5)
        holder = threading.Thread(target=hold)
        holder.start()
        self.assertTrue(entered.wait(2))
        try:
            began = time.monotonic()
            state = read_runtime_state(self.runtime, include_work=False)
            chat = self.runtime.chat_read(self.room)
            peers = self.runtime.peers(self.lead["id"])
            self.assertLess(time.monotonic() - began, 2)
            self.assertEqual(state["agents"][0]["id"], self.lead["id"])
            self.assertEqual(chat["messages"][0]["text"], "hello")
            self.assertEqual(peers["self"], self.lead["id"])
        finally:
            release.set()
            holder.join(5)

    def test_read_transaction_keeps_one_generation(self):
        with self.runtime.read_db() as db:
            before = read_runtime_state(self.runtime, include_work=False, db=db)
            with self.runtime.lock, self.runtime.db() as writer:
                agent = self.runtime.agent(self.lead["id"], writer)
                agent["name"] = "After snapshot"
                self.runtime.put(writer, "agents", agent)
            again = read_runtime_state(self.runtime, include_work=False, db=db)
            self.assertEqual(before["agents"][0]["name"], again["agents"][0]["name"])
        self.assertEqual(read_runtime_state(self.runtime, include_work=False)["agents"][0]["name"],
                         "After snapshot")

    def test_old_read_snapshot_cannot_poison_current_agent_cache(self):
        with self.runtime.read_db() as old_reader:
            old_reader.execute("SELECT count(*) FROM runtime_agents").fetchone()
            with self.runtime.db() as writer:
                writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.pinned',json('true')) WHERE id=?",
                               (self.lead["id"],))
            self.runtime.invalidate_agent_records()

            old_rows = self.runtime.records(old_reader, "agents", shared=True)
            self.assertFalse(next(row for row in old_rows if row["id"] == self.lead["id"])
                             .get("pinned", False))

        with self.runtime.read_db() as fresh_reader:
            fresh_rows = self.runtime.records(fresh_reader, "agents", shared=True)
        self.assertTrue(next(row for row in fresh_rows if row["id"] == self.lead["id"])
                        .get("pinned", False))

    def test_autocommit_agent_read_bypasses_shared_snapshot_cache(self):
        with self.runtime.read_db() as db:
            self.runtime.records(db, "agents", shared=True)
        with self.runtime.db() as writer:
            writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.pinned',json('true')) WHERE id=?",
                           (self.lead["id"],))
        self.runtime.invalidate_agent_records()

        with closing(sqlite3.connect(self.runtime.db_path)) as db:
            db.row_factory = sqlite3.Row
            db.execute("PRAGMA query_only=ON")
            generation = db.execute(
                "SELECT value FROM runtime_agent_record_generation WHERE id=1").fetchone()[0]
            rows = self.runtime.records(db, "agents", shared=True)
        self.assertTrue(next(row for row in rows if row["id"] == self.lead["id"])
                        .get("pinned", False))
        self.assertNotIn(generation, self.runtime._agent_records_cache)

    def test_agent_cache_build_does_not_block_writer_or_publish_old_revision(self):
        reading = threading.Event()
        release = threading.Event()
        writer_entered = threading.Event()
        writer_done = threading.Event()
        rows = []
        errors = []
        generation = []
        runtime = self.runtime

        class SlowRows:
            def __init__(self, cursor):
                self.cursor = cursor

            def __iter__(self):
                reading.set()
                if not release.wait(5):
                    raise RuntimeError("agent cache fixture did not release its cursor")
                return iter(self.cursor)

        class SlowRead:
            def __init__(self, db):
                self.db = db

            @property
            def in_transaction(self):
                return self.db.in_transaction

            def execute(self, sql, *args):
                cursor = self.db.execute(sql, *args)
                if sql == "SELECT record FROM runtime_agents":
                    return SlowRows(cursor)
                return cursor

        def read():
            try:
                with runtime.read_db() as db:
                    generation.append(db.execute(
                        "SELECT value FROM runtime_agent_record_generation WHERE id=1").fetchone()[0])
                    rows.extend(runtime.records(SlowRead(db), "agents", shared=True))
            except BaseException as error:
                errors.append(error)

        def write():
            try:
                with runtime.lock, runtime.db() as db:
                    writer_entered.set()
                    agent = runtime.agent(self.lead["id"], db)
                    agent["name"] = "After cache build"
                    runtime.put(db, "agents", agent)
                writer_done.set()
            except BaseException as error:
                errors.append(error)

        reader = threading.Thread(target=read)
        writer = threading.Thread(target=write)
        runtime.invalidate_agent_records()
        reader.start()
        try:
            self.assertTrue(reading.wait(2))
            writer.start()
            self.assertTrue(writer_entered.wait(2))
            self.assertTrue(writer_done.wait(1), "agent cache build blocked the SQLite writer")
            available = runtime.lock.acquire(timeout=1)
            self.assertTrue(available, "agent cache build blocked another Runtime caller")
            if available:
                runtime.lock.release()
        finally:
            release.set()
            reader.join(5)
            if writer.ident is not None:
                writer.join(5)
        self.assertFalse(reader.is_alive())
        self.assertFalse(writer.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(rows[0]["name"], "Lead")
        self.assertNotIn(generation[0], runtime._agent_records_cache)
        with runtime.read_db() as db:
            self.assertEqual(runtime.records(db, "agents", shared=True)[0]["name"],
                             "After cache build")

    def test_native_sweep_queries_use_targeted_indexes(self):
        with self.runtime.lock, self.runtime.db() as db:
            self.assertFalse(_account_busy(self.runtime, db, "default"))
            plans = []
            plans += [row[3] for row in db.execute("EXPLAIN QUERY PLAN SELECT 1 FROM runtime_agents WHERE "
                "COALESCE(json_extract(record,'$.provider'),'codex')='codex' AND "
                "COALESCE(json_extract(record,'$.accountKey'),'default')='default' AND ("
                "json_extract(record,'$.contextRepair.phase') IN "
                "('preparing','submitted','unknown','ready') OR "
                "COALESCE(json_extract(record,'$.nativeToolRefreshId'),'')!='') LIMIT 1")]
            plans += [row[3] for row in db.execute("EXPLAIN QUERY PLAN SELECT record FROM runtime_agents WHERE "
                "json_extract(record,'$.threadId')='thread' AND "
                "CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' "
                "ELSE json_extract(record,'$.accountKey') END='default'")]
            self.assertTrue(any("runtime_agent_sweep_busy" in plan for plan in plans), plans)
            self.assertTrue(any("runtime_agent_native_scope" in plan for plan in plans), plans)

    def test_fast_lock_owners_are_a_json_array(self):
        with self.runtime.db() as db:
            self.runtime.mark_event_timings(db, ["fixture-event"],
                                            {"fastLockOwners": ["41ms:codex_runtime|records"]})
            row = db.execute("SELECT record FROM runtime_event_meta WHERE id='fixture-event'").fetchone()
            self.assertEqual(json.loads(row[0])["timing"]["fastLockOwners"],
                             ["41ms:codex_runtime|records"])


if __name__ == "__main__":
    unittest.main()
