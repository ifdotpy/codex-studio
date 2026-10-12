#!/usr/bin/env python3
"""A preflight busy retry replaces the callback connection before applying work."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_waits import HANG_GUARD_SECONDS
from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import io
import json
import queue
import sqlite3
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_runtime
from codex_runtime import AppServer, Runtime


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
                if not proceed.wait(HANG_GUARD_SECONDS):
                    raise AssertionError("test did not release the notification callback")
                runtime.notification(message)
                callback_result["connectionAfterNotification"] = local.connection
                callback_result["busyReplacements"] = getattr(
                    local, "callback_db_busy_replacements", 0)
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
                self.assertTrue(notification_started.wait(HANG_GUARD_SECONDS))
                blocker = sqlite3.connect(runtime.db_path, timeout=1)
                try:
                    blocker.execute("BEGIN IMMEDIATE")
                    proceed.set()
                    self.assertTrue(busy_preflight.wait(HANG_GUARD_SECONDS), "preflight UPDATE did not hit SQLITE_BUSY")
                finally:
                    blocker.rollback()
                    blocker.close()
                self.assertTrue(callback_finished.wait(HANG_GUARD_SECONDS))
            finally:
                proceed.set()
                worker.join(5)
                self.assertFalse(worker.is_alive())

        self.assertEqual(callback_error, [])
        self.assertEqual(len(opened), 2)
        self.assertIs(callback_result["initialConnection"], opened[0])
        self.assertIs(callback_result["connectionAfterNotification"], opened[1])
        self.assertEqual(callback_result["busyReplacements"], 1)
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

    def test_busy_cursor_commit_retries_without_replaying_a_fixed_batch(self):
        temporary = tempfile.TemporaryDirectory(prefix="notification-commit-retry-")
        self.addCleanup(temporary.cleanup)
        directory = temporary.name
        scheduler_release = threading.Event()
        scheduler_started = threading.Event()

        def held_schedule(runtime):
            scheduler_started.set()
            if not scheduler_release.wait(HANG_GUARD_SECONDS):
                raise AssertionError("test did not release the live scheduler")

        with patch.object(Runtime, "schedule", held_schedule):
            runtime = Runtime(Path(directory), server_factory=object)
        self.addCleanup(runtime.close)
        self.addCleanup(scheduler_release.set)
        self.assertTrue(scheduler_started.wait(HANG_GUARD_SECONDS))
        agent = runtime.create({"name": "Fixture", "cwd": directory, "prompt": ""},
                               draft=True, defer=True)
        with runtime.lock, runtime.db() as db:
            current = runtime.agent(agent["id"], db)
            current.update(threadId="fixture-thread", turnId="fixture-turn",
                           status="running", inFlight=True, autoWake=True, events=0)
            runtime.put(db, "agents", current)

        with runtime.read_db() as db:
            before_events = json.loads(db.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent["id"],)
            ).fetchone()[0])["events"]

        messages = []
        for sequence, (method, item_id, text_value) in enumerate((
                ("item/started", "first", ""), ("item/completed", "first", "first response"),
                ("item/started", "second", ""), ("item/completed", "second", "second response")), 1):
            messages.append({
                "method": method,
                "params": {"threadId": "fixture-thread", "turnId": "fixture-turn",
                           "item": {"id": item_id, "type": "agentMessage", "text": text_value}},
                "_studioSupervisorSequence": sequence,
            })

        opened = []
        injected_busy = threading.Event()
        injected = False
        replacements = []
        callback_sequences = []
        acknowledgements = []
        all_acked = threading.Event()
        original_connect = codex_runtime.sqlite_connect

        def tracked_connect(*args, **kwargs):
            db = original_connect(*args, **kwargs)
            local = runtime.__dict__.get("_callback_db")
            if local is not None and getattr(local, "reuse", False) and not getattr(local, "depth", 0):
                opened.append(db)
                original_execute = db.execute

                def execute(sql, parameters=(), /):
                    nonlocal injected
                    if (not injected and "INSERT INTO runtime_supervisor_cursor VALUES" in str(sql)):
                        injected = True
                        injected_busy.set()
                        error = sqlite3.OperationalError("database is locked")
                        error.sqlite_errorcode = sqlite3.SQLITE_BUSY
                        raise error
                    return original_execute(sql, parameters)

                db.execute = execute
            return db

        def notify(message):
            callback_sequences.append(message["_studioSupervisorSequence"])
            runtime.notification(message)

        server = AppServer.__new__(AppServer)
        server.supervisor_mode = True
        server.lock = threading.RLock()
        server.callback_lock = threading.RLock()
        server.callbacks = queue.Queue(maxsize=AppServer.CALLBACK_QUEUE_LIMIT)
        server.dispatch_stopped = False
        server.reader_done = threading.Event()
        server.dispatcher_done = threading.Event()
        server.closed = True
        server.transport_error = None
        server.log = io.BytesIO()
        server.close_log_if_idle = lambda: None
        server.died = lambda: None
        server.notification = notify
        server.request = lambda _message: None
        server.supervisor_event_applied = lambda sequence: runtime.supervisor_event_applied(
            "fixture-handle", sequence)
        server.supervisor_commit = lambda message, sequence: runtime.commit_supervisor_event(
            "fixture-handle", message, sequence, "default", None)

        class Journal:
            def poll(self):
                return None

            def ack_applied_deltas(self, _lookup):
                return 0

            def ack(self, sequence):
                acknowledgements.append(sequence)
                if len(acknowledgements) == len(messages):
                    all_acked.set()

        server.proc = Journal()

        def dispatcher_closed():
            local = runtime.__dict__.get("_callback_db")
            replacements.append(getattr(local, "callback_db_busy_replacements", 0))
            runtime.close_notification_database()

        server.notification_dispatcher_closed = dispatcher_closed

        with patch.object(codex_runtime, "sqlite_connect", tracked_connect):
            for message in messages:
                server.enqueue(notify, message)
            dispatcher = threading.Thread(target=server.dispatch, daemon=True)
            dispatcher.start()
            try:
                self.assertTrue(all_acked.wait(HANG_GUARD_SECONDS), "dispatcher did not apply the fixed batch")
                self.assertTrue(injected_busy.is_set(), "cursor persistence did not hit the forced busy error")
                server.reader_done.set()
                dispatcher.join(HANG_GUARD_SECONDS)
                self.assertFalse(dispatcher.is_alive())
            finally:
                server.reader_done.set()
                if dispatcher.is_alive():
                    dispatcher.join(3)

        self.assertTrue(injected)
        self.assertEqual(callback_sequences, [1, 2, 3, 4])
        self.assertEqual(acknowledgements, [1, 2, 3, 4])
        self.assertEqual(len(opened), 2)
        self.assertEqual(replacements, [1])
        with runtime.read_db() as db:
            record = json.loads(db.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent["id"],)
            ).fetchone()[0])
            items = db.execute(
                "SELECT id, record FROM runtime_items WHERE agent=? ORDER BY id", (agent["id"],)
            ).fetchall()
            cursor = db.execute(
                "SELECT sequence FROM runtime_supervisor_cursor WHERE handle='fixture-handle'"
            ).fetchone()
        self.assertEqual(record["events"], before_events + 4)
        self.assertEqual([row[0] for row in items], [agent["id"] + ":first", agent["id"] + ":second"])
        self.assertEqual([json.loads(row[1])["text"] for row in items],
                         ["first response", "second response"])
        self.assertEqual(cursor[0], 4)


if __name__ == "__main__":
    unittest.main()
