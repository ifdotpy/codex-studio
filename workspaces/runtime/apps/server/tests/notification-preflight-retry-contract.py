#!/usr/bin/env python3
"""A preflight busy retry replaces the callback connection before applying work."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import sqlite3
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_runtime
from codex_runtime import Runtime


class QuietRuntime(Runtime):
    def schedule(self):
        return


class NotificationPreflightRetry(unittest.TestCase):
    def test_busy_preflight_replaces_connection_before_one_supervisor_application(self):
        temporary = tempfile.TemporaryDirectory(prefix="notification-preflight-retry-")
        self.addCleanup(temporary.cleanup)
        directory = temporary.name
        runtime = QuietRuntime(Path(directory), server_factory=object)
        self.addCleanup(runtime.close)
        agent = runtime.create({"name": "Fixture", "cwd": directory, "prompt": ""},
                               draft=True, defer=True)
        with runtime.lock, runtime.db() as db:
            current = runtime.agent(agent["id"], db)
            current.update(threadId="fixture-thread", turnId="fixture-turn",
                           status="running", inFlight=True, autoWake=True)
            runtime.put(db, "agents", current)
        with runtime.read_db() as db:
            before_events = json.loads(db.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent["id"],)
            ).fetchone()[0])["events"]

        opened = []
        busy_preflight = threading.Event()
        notification_started = threading.Event()
        proceed = threading.Event()
        callback_finished = threading.Event()
        callback_error = []
        callback_result = {}
        original_connect = codex_runtime.sqlite_connect

        def tracked_connect(*args, **kwargs):
            db = original_connect(*args, **kwargs)
            local = runtime.__dict__.get("_callback_db")
            if local is not None and getattr(local, "reuse", False) and not getattr(local, "depth", 0):
                opened.append(db)
                original_execute = db.execute

                def execute(sql, parameters=(), /):
                    try:
                        return original_execute(sql, parameters)
                    except sqlite3.OperationalError as error:
                        if ("UPDATE runtime_supervisor_cursor SET sequence=sequence" in str(sql)
                                and codex_runtime.sqlite_busy(error)):
                            busy_preflight.set()
                        raise

                db.execute = execute
            return db

        message = {
            "method": "item/started",
            "params": {"threadId": "fixture-thread", "turnId": "fixture-turn",
                       "item": {"id": "retry-item", "type": "agentMessage", "text": "hello"}},
            "_studioDispatchedAt": time.time(),
            "_studioSupervisorSequence": 41,
        }

        def apply_notification():
            local = runtime.__dict__.setdefault("_callback_db", threading.local())
            local.reuse = True
            try:
                with runtime.db():
                    pass
                callback_result["initialConnection"] = local.connection
                notification_started.set()
                if not proceed.wait(3):
                    raise AssertionError("test did not release the notification callback")
                runtime.notification(message)
                callback_result["connectionAfterNotification"] = local.connection
                callback_result["preflightRetries"] = getattr(
                    local, "notification_preflight_busy_retries", 0)
                runtime.commit_supervisor_event("fixture-handle", message, 41,
                                                "default", None)
            except BaseException as error:
                callback_error.append(error)
            finally:
                callback_finished.set()

        with patch.object(codex_runtime, "sqlite_connect", tracked_connect):
            worker = threading.Thread(target=apply_notification)
            worker.start()
            try:
                self.assertTrue(notification_started.wait(3))
                blocker = sqlite3.connect(runtime.db_path, timeout=1)
                try:
                    blocker.execute("BEGIN IMMEDIATE")
                    proceed.set()
                    self.assertTrue(busy_preflight.wait(3), "preflight UPDATE did not hit SQLITE_BUSY")
                finally:
                    blocker.rollback()
                    blocker.close()
                self.assertTrue(callback_finished.wait(5))
            finally:
                proceed.set()
                worker.join(5)
                self.assertFalse(worker.is_alive())

        self.assertEqual(callback_error, [])
        self.assertEqual(len(opened), 2)
        self.assertIs(callback_result["initialConnection"], opened[0])
        self.assertIs(callback_result["connectionAfterNotification"], opened[1])
        self.assertEqual(callback_result["preflightRetries"], 1)
        with runtime.read_db() as db:
            record = json.loads(db.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent["id"],)
            ).fetchone()[0])
            item_count = db.execute(
                "SELECT COUNT(*) FROM runtime_items WHERE id=?", (agent["id"] + ":retry-item",)
            ).fetchone()[0]
            cursor = db.execute(
                "SELECT sequence FROM runtime_supervisor_cursor WHERE handle='fixture-handle'"
            ).fetchone()
        self.assertEqual(record["events"], before_events + 1)
        self.assertEqual(item_count, 1)
        self.assertEqual(cursor[0], 41)


if __name__ == "__main__":
    unittest.main()
