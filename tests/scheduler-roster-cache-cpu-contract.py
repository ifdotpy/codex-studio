#!/usr/bin/env python3
"""Roster cache dependencies exclude unrelated writes and retain transaction scope."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import ast
from contextlib import contextmanager, ExitStack
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import Runtime

if os.environ.get("CODEX_SCHEDULER_CACHE_SOURCE"):
    tree = ast.parse(Path(os.environ["CODEX_SCHEDULER_CACHE_SOURCE"]).read_text())
    owner = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == "Runtime")
    for name in ("scheduler_roster_key", "scheduler_agents", "dispatch_all"):
        node = next((node for node in owner.body if isinstance(node, ast.FunctionDef) and node.name == name), None)
        if node is None:
            if name == "scheduler_roster_key":
                continue
            raise AssertionError("The frozen scheduler method is missing")
        namespace = {}
        exec(compile(ast.Module(body=[node], type_ignores=[]), "<frozen-scheduler>", "exec"),
             Runtime.scheduler_agents.__globals__, namespace)
        setattr(Runtime, name, namespace[name])


class RosterCache(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name) / "canvas.sqlite3"
        self.connections = []
        self.scans = []
        self.runtime = Runtime.__new__(Runtime)
        self.runtime.closed = False
        self.runtime.lock = threading.RLock()
        self.runtime._agent_record_revision = 0
        db = self.connect()
        db.execute("PRAGMA journal_mode=WAL")
        db.executescript("""
            CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE sync_generation(id INTEGER PRIMARY KEY,value INTEGER NOT NULL);
            INSERT INTO sync_generation VALUES(1,0);
            CREATE TABLE runtime_agent_record_generation(id INTEGER PRIMARY KEY CHECK(id=1),value INTEGER NOT NULL);
            INSERT INTO runtime_agent_record_generation VALUES(1,0);
            CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents BEGIN
                UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1;
            END;
            CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE OF record ON runtime_agents
                WHEN OLD.record IS NOT NEW.record BEGIN
                UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1;
            END;
            CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents BEGIN
                UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1;
            END;
            CREATE INDEX runtime_work_owner_status ON runtime_work(json_extract(record,'$.owner'),json_extract(record,'$.status'));
            CREATE INDEX runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'));
            INSERT INTO runtime_tasks VALUES('task','{"id":"task","status":"running"}');
        """)
        db.commit()
        self.db = db

    def tearDown(self):
        for db in self.connections:
            db.close()
        self.tmp.cleanup()

    def connect(self):
        db = sqlite3.connect(self.path)
        db.row_factory = sqlite3.Row
        db.set_trace_callback(lambda sql: self.scans.append(sql)
                              if sql.startswith("SELECT id,record") and "FROM runtime_agents WHERE" in sql else None)
        self.connections.append(db)
        return db

    def add(self, key="agent", **fields):
        agent = {"id": key, "rootId": "root", "parentId": "root", "isLead": False,
                 "name": key, "epoch": 1, "status": "completed", "inFlight": False,
                 "autoWake": False, "metadata": {"list": [1]}}
        agent.update(fields)
        self.db.execute("INSERT INTO runtime_agents VALUES(?,?)", (key, json.dumps(agent)))
        self.db.commit()
        return agent

    def roster(self, db=None):
        return self.runtime.scheduler_agents(db or self.db)

    def ids(self, db=None):
        return {agent["id"] for agent in self.roster(db)}

    def test_unrelated_write_reuses_committed_roster_and_preserves_copies(self):
        self.add(status="running")
        expected = tuple(self.roster())
        expected[0]["metadata"]["list"].append(99)
        scans = len(self.scans)
        self.db.execute("UPDATE runtime_tasks SET record=json_set(record,'$.status','completed')")
        actual = self.roster()
        self.assertEqual(actual[0]["metadata"], {"list": [1]})
        self.assertEqual(len(self.scans), scans)
        self.db.commit()
        self.assertEqual(self.roster(self.connect()), actual)
        self.assertEqual(len(self.scans), scans)

    def test_uncommitted_roster_stays_on_exact_connection_and_rollback_generation_can_repeat(self):
        self.add()
        self.assertEqual(self.ids(), set())
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','running')")
        self.assertEqual(self.roster()[0]["status"], "running")
        first_generation = self.db.execute("SELECT value FROM runtime_agent_record_generation").fetchone()[0]
        self.assertEqual(self.ids(self.connect()), set())
        self.db.rollback()
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")
        self.assertEqual(self.db.execute("SELECT value FROM runtime_agent_record_generation").fetchone()[0], first_generation)
        self.assertEqual(self.roster()[0]["status"], "approval")
        self.db.commit()
        self.assertEqual(self.roster(self.connect())[0]["status"], "approval")

    def test_direct_agent_insert_update_delete_changes_membership(self):
        self.add(status="running")
        self.assertEqual(self.ids(), {"agent"})
        writer = self.connect()
        writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','changed')")
        writer.commit()
        self.assertEqual(self.roster(self.connect())[0]["name"], "changed")
        writer.execute("DELETE FROM runtime_agents")
        writer.commit()
        self.assertEqual(self.ids(self.connect()), set())
        self.add("replacement", status="queued", autoWake=True)
        self.assertEqual(self.ids(self.connect()), {"replacement"})

    def test_rollback_then_other_writer_reuses_generation_and_same_connection_change_count(self):
        self.add()
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','running')")
        generation = self.db.execute("SELECT value FROM runtime_agent_record_generation").fetchone()[0]
        changes = self.db.total_changes
        self.assertEqual(self.roster()[0]["status"], "running")
        self.db.rollback()
        writer = self.connect()
        writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")
        writer.commit()
        self.db.execute("BEGIN")
        self.assertEqual(self.db.total_changes, changes)
        self.assertEqual(self.db.execute("SELECT value FROM runtime_agent_record_generation").fetchone()[0], generation)
        self.assertEqual(self.roster()[0]["status"], "approval")
        self.db.rollback()

    def test_work_owner_membership_changes_without_agent_write(self):
        self.add("first", status="failed")
        self.add("second", deletedAt=1)
        self.assertEqual(self.ids(), set())
        writer = self.connect()
        writer.execute("INSERT INTO runtime_work VALUES('work',?)", (json.dumps({"owner": "first", "status": "ready"}),))
        writer.commit()
        self.assertEqual(self.ids(self.connect()), {"first"})
        writer.execute("UPDATE runtime_work SET record=json_set(record,'$.owner','second','$.status','blocked')")
        writer.commit()
        self.assertEqual(self.ids(self.connect()), {"second"})
        writer.execute("UPDATE runtime_work SET record=json_set(record,'$.status','accepted')")
        writer.commit()
        self.assertEqual(self.ids(self.connect()), set())

    def test_pending_transfer_keeps_full_root_roster_and_changes_without_agent_write(self):
        self.add("lead", rootId="lead", parentId=None, isLead=True)
        self.add("child", rootId="lead", parentId="lead")
        self.add("other", rootId="other")
        self.assertEqual(self.ids(), set())
        writer = self.connect()
        writer.execute("INSERT INTO runtime_account_transfers VALUES('transfer',?)", (json.dumps({"leadId": "lead", "status": "pending"}),))
        writer.commit()
        self.assertEqual(self.ids(self.connect()), {"lead", "child"})
        writer.execute("UPDATE runtime_account_transfers SET record=json_set(record,'$.leadId','other')")
        writer.commit()
        self.assertEqual(self.ids(self.connect()), {"other"})
        writer.execute("UPDATE runtime_account_transfers SET record=json_set(record,'$.status','completed')")
        writer.commit()
        self.assertEqual(self.ids(self.connect()), set())

    def test_missing_counter_uses_uncached_fallback(self):
        self.add(status="running")
        self.db.executescript("""
            DROP TRIGGER runtime_agent_record_generation_insert;
            DROP TRIGGER runtime_agent_record_generation_update;
            DROP TRIGGER runtime_agent_record_generation_delete;
            DROP TABLE runtime_agent_record_generation;
        """)
        self.assertEqual(self.roster()[0]["status"], "running")
        scans = len(self.scans)
        self.assertEqual(self.roster()[0]["status"], "running")
        self.assertEqual(len(self.scans), scans + 1)
        writer = self.connect()
        writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")
        writer.commit()
        self.assertEqual(self.roster(self.connect())[0]["status"], "approval")

    def test_old_wal_snapshot_does_not_poison_current_roster(self):
        self.add(status="running")
        old = self.connect()
        old.execute("BEGIN")
        self.assertEqual(self.roster(old)[0]["status"], "running")
        self.db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")
        self.db.commit()
        self.assertEqual(self.roster()[0]["status"], "approval")
        self.assertEqual(self.roster(old)[0]["status"], "running")
        self.assertEqual(self.roster(self.connect())[0]["status"], "approval")
        old.rollback()

    def test_old_roster_tuple_is_not_a_new_cache_entry(self):
        self.add(status="running")
        self.runtime._scheduler_agent_roster = (self.db, 0, 0, self.db.total_changes,
                                                ({"id": "foreign-old-row"},))
        self.assertEqual(self.ids(), {"agent"})

    def test_key_change_after_row_read_does_not_publish_wrong_generation(self):
        self.add(status="running")
        writer = self.connect()
        inner = self.db
        changed = False

        class RowCursor:
            def __init__(self, cursor):
                self.cursor = cursor

            def fetchall(self):
                nonlocal changed
                rows = self.cursor.fetchall()
                if not changed:
                    changed = True
                    writer.execute("UPDATE runtime_agents SET record=json_set(record,'$.name','new')")
                    writer.commit()
                return rows

        class Connection:
            def __getattr__(self, name):
                return getattr(inner, name)

            def execute(self, sql, *args):
                cursor = inner.execute(sql, *args)
                return RowCursor(cursor) if sql.startswith("SELECT id,record") and "FROM runtime_agents WHERE" in sql else cursor

        self.assertEqual(self.roster(Connection())[0]["name"], "agent")
        self.assertEqual(self.roster()[0]["name"], "new")
        self.assertEqual(len(self.scans), 2)

    def dispatch_with_unrelated_write(self, before_read=None):
        self.add(status="running")
        rt = self.runtime
        calls = []

        @contextmanager
        def database():
            db = self.connect()
            try:
                yield db
                db.commit()
            except BaseException:
                db.rollback()
                raise

        rt.db = database
        rt.analytics_history_ensure_running = lambda: None
        rt.accepted_archive_tick = lambda: None
        rt.retry_monitor_results = lambda: None

        def recover(_rt, db, agents):
            db.execute("UPDATE runtime_tasks SET record=json_set(record,'$.status','completed')")

        def candidates(_key, current):
            with database() as db:
                self.assertEqual({agent["id"] for agent in current(db)}, {"agent"})
                db.execute("UPDATE runtime_tasks SET record=json_set(record,'$.status','running')")
                self.assertEqual({agent["id"] for agent in current(db)}, {"agent"})
            return 0

        rt.dispatch_candidates = candidates
        original = rt.scheduler_agents

        def roster(db):
            calls.append(db)
            return original(db)

        with ExitStack() as stack:
            for name in ("codex_claude_auth_wait.tick", "codex_native_runtime.tick", "codex_provider_versions.tick",
                         "codex_native_release.tick", "codex_team_isolation.cancel_pending",
                         "codex_context_repair.tick_restart_input_waits"):
                stack.enter_context(patch(name, lambda *_args: None))
            stack.enter_context(patch("codex_radio.tick", before_read or (lambda *_args: None)))
            stack.enter_context(patch("codex_session_names.session_names",
                                      lambda _rt: type("Names", (), {"tick": lambda self: None})()))
            stack.enter_context(patch("codex_context_repair.recover_context_failures", recover))
            stack.enter_context(patch.object(rt, "scheduler_agents", side_effect=roster))
            self.assertEqual(rt.dispatch_all(), 0)
        return calls

    def test_dispatch_reuses_roster_after_unrelated_write(self):
        calls = self.dispatch_with_unrelated_write()
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(self.scans), 1)

    def test_dispatch_does_not_reuse_uncommitted_decoded_roster(self):
        def before_read(_runtime, db):
            db.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','approval')")

        calls = self.dispatch_with_unrelated_write(before_read)
        self.assertEqual(len(calls), 3)
        self.assertEqual(len(self.scans), 3)
        self.assertEqual(self.roster(self.connect())[0]["status"], "approval")

    def test_private_large_roster_cpu_and_select_counts(self):
        records = []
        for index in range(2048):
            agent = {"id": str(index), "rootId": "root", "parentId": "root", "isLead": False,
                     "name": str(index), "epoch": 1, "status": "running" if index < 64 else "completed",
                     "autoWake": False, "inFlight": index < 64, "prompt": "x" * 4096,
                     "lastCompletedTurn": "turn", "lastCompletedTurnStatus": "completed"}
            if index >= 64:
                agent.update(restartRecovery={"stage": "finished"}, contextRepair={"phase": "completed"},
                             startAttempt={"submitted": True, "turnId": "turn"})
            records.append((str(index), json.dumps(agent)))
        self.db.executemany("INSERT INTO runtime_agents VALUES(?,?)", records)
        self.db.commit()
        started = time.thread_time()
        expected = tuple(self.roster())
        cold_cpu = time.thread_time() - started
        scans = len(self.scans)
        started = time.thread_time()
        for index in range(8):
            self.db.execute("UPDATE runtime_tasks SET record=json_set(record,'$.iteration',?)", (index,))
            self.assertEqual(tuple(self.roster()), expected)
        warm_cpu = time.thread_time() - started
        print(json.dumps({"privateRows": len(records), "selected": len(expected),
                          "coldCpuMs": round(cold_cpu * 1000, 3), "eightUnrelatedWritesCpuMs": round(warm_cpu * 1000, 3),
                          "fullRosterScansAfterWrites": len(self.scans) - scans}), flush=True)
        self.assertEqual(len(expected), 64)
        self.assertEqual(len(self.scans), scans)


if __name__ == "__main__":
    unittest.main()
