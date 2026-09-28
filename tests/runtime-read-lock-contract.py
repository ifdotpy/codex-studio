#!/usr/bin/env python3
"""Read views keep a WAL snapshot and do not hold the runtime lock."""

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_canvas import Canvas, make_server
from codex_native_sweep import _account_busy
from codex_runtime import Runtime
from codex_sync import SyncStore

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
        SyncStore(self.runtime.db, lambda: {}, lambda _key: {})

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
        server = make_server(self.canvas, 0)
        try:
            http_snapshot = next(cell.cell_contents for cell in server.RequestHandlerClass.do_GET.__closure__
                                 if callable(cell.cell_contents) and
                                 getattr(cell.cell_contents, "__name__", None) == "snapshot")
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
                state = http_snapshot(False)
                chat = self.runtime.chat_read(self.room)
                peers = self.runtime.peers(self.lead["id"])
                self.assertLess(time.monotonic() - began, 2)
                self.assertEqual(state["runtime"]["agents"][0]["id"], self.lead["id"])
                self.assertEqual(chat["messages"][0]["text"], "hello")
                self.assertEqual(peers["self"], self.lead["id"])
            finally:
                release.set()
                holder.join(5)
        finally:
            server.server_close()

    def test_read_transaction_keeps_one_generation(self):
        with self.runtime.read_db() as db:
            before = self.runtime.snapshot(include_work=False, db=db)
            with self.runtime.lock, self.runtime.db() as writer:
                agent = self.runtime.agent(self.lead["id"], writer)
                agent["name"] = "After snapshot"
                self.runtime.put(writer, "agents", agent)
            again = self.runtime.snapshot(include_work=False, db=db)
            self.assertEqual(before["agents"][0]["name"], again["agents"][0]["name"])
        self.assertEqual(self.runtime.snapshot(include_work=False)["agents"][0]["name"],
                         "After snapshot")

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
