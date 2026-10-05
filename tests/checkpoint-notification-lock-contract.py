#!/usr/bin/env python3
"""Automatic checkpoints release notification locks before path checks."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import sys
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_source import source_function
import codex_workspace

spec = importlib.util.spec_from_file_location("workspace_fixture", ROOT / "tests/workspace-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CheckpointNotificationLockContract(unittest.TestCase):
    def setUp(self):
        baseline = os.environ.get("STUDIO_CHECKPOINT_BASELINE")
        if baseline:
            source = Path(baseline).read_bytes()
            for name in ("_reserve_checkpoint", "queue_checkpoint_after_turn", "_capture_reserved_checkpoint",
                         "checkpoint_after_turn", "_assert_workspace_idle"):
                function, _ = source_function(source, ("WorkspaceMixin", name), vars(codex_workspace), baseline)
                replacement = patch.object(codex_workspace.WorkspaceMixin, name, function)
                replacement.start()
                self.addCleanup(replacement.stop)
        fixture.WorkspaceContract.setUp(self)
        self.agent = fixture.WorkspaceContract.lead(self)
        self.agent = self.update(self.agent, autoWake=True, status="running", inFlight=True,
                                 threadId="checkpoint-thread", turnId="checkpoint-turn",
                                 worktreeReady=True, turnEpoch=self.agent["epoch"])
        self.captures = []
        self.jobs = []
        self.capture = patch.object(self.runtime, "capture_checkpoint", side_effect=self.captured)
        self.capture.start()
        self.addCleanup(self.capture.stop)
        submit = self.runtime.pool.submit

        def submitted(function, *args, **kwargs):
            future = submit(function, *args, **kwargs)
            if function.__name__ in {"_checkpoint_after_committed_turn", "checkpoint_after_turn"}:
                self.jobs.append(future)
            return future

        self.submission = patch.object(self.runtime.pool, "submit", side_effect=submitted)
        self.submission.start()
        self.addCleanup(self.submission.stop)
        with self.runtime.db() as db:
            db.execute("CREATE TABLE fixture_checkpoint_probe(id TEXT PRIMARY KEY)")

    def tearDown(self):
        fixture.WorkspaceContract.tearDown(self)

    def update(self, agent, **changes):
        return fixture.WorkspaceContract.agent_update(self, agent, **changes)

    def captured(self, key, label, turn_id=None):
        self.assertFalse(self.runtime.lock._is_owned())
        self.captures.append((key, label, turn_id))
        return {"id": "fixture-checkpoint", "agent": key}

    def notice(self):
        return {"method": "turn/completed", "params": {
            "threadId": self.agent["threadId"], "turn": {"id": self.agent["turnId"], "status": "completed"}}}

    def drain(self):
        for future in self.jobs:
            future.result(timeout=3)

    @contextmanager
    def resolving(self):
        entered, release = threading.Event(), threading.Event()
        resolve = Path.resolve

        def blocked(path, *args, **kwargs):
            if str(path) == self.agent["cwd"] and not entered.is_set():
                entered.set()
                if not release.wait(3):
                    raise TimeoutError("The fixture path gate expired")
            return resolve(path, *args, **kwargs)

        with patch.object(Path, "resolve", blocked), ThreadPoolExecutor(max_workers=1) as pool:
            notice = pool.submit(self.runtime.notification, self.notice())
            try:
                self.assertTrue(entered.wait(3), "The real checkpoint path check must reach the gate")
                yield notice
            finally:
                release.set()
                notice.result(timeout=3)
                self.drain()

    def operations(self):
        with self.runtime.db() as db:
            return [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_workspace_operations")]

    def test_blocked_resolve_releases_writer_and_runtime_lock_after_committed_turn(self):
        with self.resolving() as notice:
            acquired = self.runtime.lock.acquire(blocking=False)
            if acquired:
                self.runtime.lock.release()
            self.assertTrue(acquired, "Path.resolve must not hold Runtime.lock")
            with closing(sqlite3.connect(self.runtime.db_path, timeout=0)) as writer, writer:
                writer.execute("INSERT INTO fixture_checkpoint_probe VALUES ('during-resolve')")
                writer.commit()
                completion = self.agent["id"] + ":" + self.agent["turnId"]
                self.assertEqual(writer.execute("SELECT id FROM runtime_completed_turns WHERE id=?",
                                                (completion,)).fetchone(), (completion,))
            notice.result(timeout=1)
            receipt = self.runtime.send(self.agent["id"], "A private fixture followup")
            self.assertEqual(self.runtime.delivery_receipt(receipt["id"])["status"], "pending")
        self.assertEqual(self.captures, [(self.agent["id"], "After turn", self.agent["turnId"])])
        self.assertEqual([operation["phase"] for operation in self.operations()], ["completed"])
        self.runtime.notification(self.notice())
        self.drain()
        self.assertEqual(len(self.captures), 1, "The completed-turn receipt prevents duplicate capture")

    def test_blocked_resolve_allows_a_second_sqlite_writer(self):
        with self.resolving():
            with closing(sqlite3.connect(self.runtime.db_path, timeout=0)) as writer, writer:
                writer.execute("INSERT INTO fixture_checkpoint_probe VALUES ('other-writer')")
                writer.commit()
                self.assertEqual(writer.execute("SELECT id FROM fixture_checkpoint_probe").fetchall(),
                                 [("other-writer",)])
        self.assertEqual(self.captures, [(self.agent["id"], "After turn", self.agent["turnId"])])

    def test_rolled_back_notification_does_not_capture_or_collect_images(self):
        self.agent = self.update(self.agent, imageWorkspaceReady=True)
        put = self.runtime.put

        def failed_put(db, table, record, **kwargs):
            if table == "agents" and record.get("lastCompletedTurn") == self.agent["turnId"] and self.jobs:
                raise ValueError("fixture terminal commit failure")
            return put(db, table, record, **kwargs)

        with patch.object(self.runtime, "put", side_effect=failed_put):
            with patch("codex_workspace_images.collect") as collect:
                with self.assertRaisesRegex(ValueError, "^fixture terminal commit failure$"):
                    self.runtime.notification(self.notice())
                self.drain()
                collect.assert_not_called()
        self.assertEqual(self.captures, [])
        self.assertEqual(self.operations(), [])
        with self.runtime.db() as db:
            self.assertIsNone(db.execute("SELECT 1 FROM runtime_completed_turns WHERE id=?",
                                        (self.agent["id"] + ":" + self.agent["turnId"],)).fetchone())
        current = self.runtime.agent(self.agent["id"])
        self.assertTrue(current["inFlight"])
        self.assertEqual(current["turnId"], self.agent["turnId"])

    def test_stop_during_resolve_does_not_reserve_checkpoint(self):
        with self.resolving() as notice:
            notice.result(timeout=1)
            self.runtime.stop(self.agent["id"], False)
        current = self.runtime.agent(self.agent["id"])
        self.assertFalse(current["autoWake"])
        self.assertEqual(current["status"], "paused")
        self.assertGreater(current["epoch"], self.agent["epoch"])
        self.assertEqual(self.captures, [])
        self.assertEqual(self.operations(), [])

    def test_resumed_turn_during_resolve_keeps_new_turn_and_inputs(self):
        with self.resolving() as notice:
            notice.result(timeout=1)
            current = self.update(self.agent, inFlight=True, status="running", turnId="next-turn")
            with self.runtime.lock, self.runtime.db() as db:
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,?,?,?,?,?)",
                           ("unknown-input", current["id"], "user", "Private fixture input", "uncertain", 1, current["epoch"]))
        current = self.runtime.agent(self.agent["id"])
        self.assertTrue(current["inFlight"])
        self.assertEqual(current["turnId"], "next-turn")
        self.assertEqual(self.runtime.delivery_receipt("unknown-input")["status"], "uncertain")
        self.assertEqual(self.captures, [])
        self.assertEqual(self.operations(), [])

    def test_new_unsent_attempt_during_resolve_keeps_its_identity(self):
        attempt = {"id": "new-unsent-attempt", "submitted": False, "events": ["new-input"],
                   "epoch": self.agent["epoch"], "accountKey": self.agent["accountKey"],
                   "threadId": self.agent["threadId"], "created": 1}
        with self.resolving() as notice:
            notice.result(timeout=1)
            self.update(self.agent, status="queued", startAttempt=attempt)
        self.assertEqual(self.runtime.agent(self.agent["id"])["startAttempt"], attempt)
        self.assertEqual(self.captures, [])
        self.assertEqual(self.operations(), [])

    def test_source_change_during_resolve_skips_old_turn_checkpoint(self):
        for change in ({"threadId": "other-thread"}, {"accountKey": "other-account"}, {"cwd": str(self.root)}):
            with self.subTest(change=change):
                original = self.runtime.agent(self.agent["id"])
                self.agent = self.update(original, autoWake=True, status="running", inFlight=True,
                                         turnId="checkpoint-turn-" + next(iter(change)))
                with self.resolving() as notice:
                    notice.result(timeout=1)
                    changed = self.update(self.agent, **change)
                self.assertEqual(self.captures, [])
                self.assertEqual(self.operations(), [])
                self.agent = self.update(changed, threadId=original["threadId"],
                                         accountKey=original["accountKey"], cwd=original["cwd"])

    def test_fork_reservation_during_resolve_is_preserved(self):
        operation = {"id": "existing-fork", "kind": "branch", "agent": self.agent["id"],
                     "phase": "provider_pending", "epoch": self.agent["epoch"], "cwd": self.agent["cwd"]}
        with self.resolving() as notice:
            notice.result(timeout=1)
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(self.agent["id"], db)
                current.update(workspaceOperation="branch", workspaceReservationId=operation["id"])
                self.runtime.put(db, "agents", current)
                self.runtime._put_workspace_operation(db, operation)
        self.assertEqual(self.operations(), [operation])
        self.assertEqual(self.runtime.agent(self.agent["id"])["workspaceReservationId"], operation["id"])
        self.assertEqual(self.captures, [])

    def test_monitor_change_during_resolve_rejects_stale_idle_snapshot(self):
        monitor = {"id": "new-monitor", "agent": self.agent["id"], "cwd": self.agent["cwd"], "status": "running"}
        with self.resolving() as notice:
            notice.result(timeout=1)
            with self.runtime.lock, self.runtime.db() as db:
                db.execute("INSERT INTO runtime_monitors VALUES (?,?)", (monitor["id"], json.dumps(monitor)))
        self.assertEqual(self.captures, [])
        self.assertEqual(self.operations(), [])
        self.assertIn("Workspace activity changed", self.runtime.agent(self.agent["id"])["checkpointError"])
        with self.runtime.db() as db:
            self.assertEqual(json.loads(db.execute("SELECT record FROM runtime_monitors WHERE id=?",
                                                  (monitor["id"],)).fetchone()[0]), monitor)

    def test_symlink_peer_activity_still_blocks_checkpoint(self):
        alias = self.root / "workspace-alias"
        alias.symlink_to(self.project, target_is_directory=True)
        peer = fixture.WorkspaceContract.lead(self, "Peer")
        self.update(peer, cwd=str(alias), inFlight=True, status="running", autoWake=True, threadId="peer-thread")
        self.runtime.notification(self.notice())
        self.drain()
        self.assertEqual(self.captures, [])
        self.assertEqual(self.operations(), [])
        self.assertIn("An agent is using this workspace", self.runtime.agent(self.agent["id"])["checkpointError"])

    def test_reservation_capture_rechecks_epoch_after_second_path_check(self):
        self.agent = self.update(self.agent, inFlight=False, status="completed", turnId=None)
        with self.runtime.lock, self.runtime.db() as db:
            operation_id = self.runtime._reserve_checkpoint(db, self.agent, "checkpoint", "old-turn")
        entered, release = threading.Event(), threading.Event()
        resolve = Path.resolve

        def blocked(path, *args, **kwargs):
            entered.set()
            if not release.wait(3):
                raise TimeoutError("The fixture reservation gate expired")
            return resolve(path, *args, **kwargs)

        with patch.object(Path, "resolve", blocked), ThreadPoolExecutor(max_workers=1) as pool:
            captured = pool.submit(self.runtime._capture_reserved_checkpoint,
                                   self.agent["id"], "After turn", "old-turn", operation_id)
            try:
                self.assertTrue(entered.wait(3))
                self.runtime.stop(self.agent["id"], False)
            finally:
                release.set()
                self.assertIsNone(captured.result(timeout=3))
        self.assertEqual(self.captures, [])
        self.assertEqual(self.operations()[0]["phase"], "failed")
        self.assertFalse(self.runtime.agent(self.agent["id"])["autoWake"])

    def image_operation(self):
        self.agent = self.update(self.agent, inFlight=False, turnId=None, status="completed",
                                 imageWorkspaceReady=True, lastCompletedTurn="image-turn")
        with self.runtime.lock, self.runtime.db() as db:
            return self.runtime._reserve_checkpoint(db, self.agent, "checkpoint", "image-turn")

    def test_stop_during_capture_prevents_image_collection(self):
        operation_id = self.image_operation()
        entered, release = threading.Event(), threading.Event()

        def capture(*args):
            entered.set()
            self.assertTrue(release.wait(3))
            return {"id": "successful-capture"}

        with patch.object(self.runtime, "capture_checkpoint", side_effect=capture):
            with patch("codex_workspace_images.collect") as collect, ThreadPoolExecutor(max_workers=1) as pool:
                future = pool.submit(self.runtime.checkpoint_after_turn, self.agent["id"], "image-turn", operation_id)
                try:
                    self.assertTrue(entered.wait(3))
                    self.runtime.stop(self.agent["id"], False)
                finally:
                    release.set()
                    future.result(timeout=3)
                collect.assert_not_called()
        current = self.runtime.agent(self.agent["id"])
        self.assertFalse(current["autoWake"])
        self.assertGreater(current["epoch"], self.agent["epoch"])
        self.assertNotIn("imageWorkspaceCollect", current)

    def test_source_change_during_capture_prevents_image_collection(self):
        for change in ({"threadId": "changed-thread"}, {"accountKey": "changed-account"}, {"cwd": str(self.root)}):
            with self.subTest(change=change):
                original = self.runtime.agent(self.agent["id"])
                operation_id = self.image_operation()
                entered, release = threading.Event(), threading.Event()

                def capture(*args):
                    entered.set()
                    self.assertTrue(release.wait(3))
                    return {"id": "successful-capture"}

                with patch.object(self.runtime, "capture_checkpoint", side_effect=capture):
                    with patch("codex_workspace_images.collect") as collect, ThreadPoolExecutor(max_workers=1) as pool:
                        future = pool.submit(self.runtime.checkpoint_after_turn, self.agent["id"], "image-turn", operation_id)
                        try:
                            self.assertTrue(entered.wait(3))
                            changed = self.update(self.agent, **change)
                        finally:
                            release.set()
                            future.result(timeout=3)
                        collect.assert_not_called()
                self.assertNotIn("imageWorkspaceCollect", self.runtime.agent(self.agent["id"]))
                self.agent = self.update(changed, threadId=original["threadId"],
                                         accountKey=original["accountKey"], cwd=original["cwd"])

    def test_stop_during_started_collect_does_not_write_stale_result_or_error(self):
        for fail in (False, True):
            with self.subTest(fail=fail):
                self.agent = self.update(self.agent, autoWake=True, imageWorkspaceError="saved image notice")
                operation_id = self.image_operation()
                entered, release = threading.Event(), threading.Event()

                def collect(*args):
                    entered.set()
                    self.assertTrue(release.wait(3))
                    if fail:
                        raise ValueError("fixture collect failure")
                    return {"conflict": "fixture conflict", "rawRef": "fixture-ref"}

                with patch("codex_workspace_images.collect", side_effect=collect) as collected:
                    with patch.object(self.runtime, "parent_event") as parent, ThreadPoolExecutor(max_workers=1) as pool:
                        future = pool.submit(self.runtime.checkpoint_after_turn, self.agent["id"], "image-turn", operation_id)
                        try:
                            self.assertTrue(entered.wait(3))
                            self.runtime.stop(self.agent["id"], False)
                        finally:
                            release.set()
                            future.result(timeout=3)
                        collected.assert_called_once_with(self.agent["id"])
                        parent.assert_not_called()
                current = self.runtime.agent(self.agent["id"])
                self.assertFalse(current["autoWake"])
                self.assertNotIn("imageWorkspaceCollect", current)
                self.assertEqual(current["imageWorkspaceError"], "saved image notice")
                self.agent = current

    def test_failed_checkpoint_does_not_start_image_collection(self):
        operation_id = self.image_operation()
        with patch.object(self.runtime, "capture_checkpoint", side_effect=ValueError("fixture capture failure")):
            with patch("codex_workspace_images.collect") as collect:
                self.runtime.checkpoint_after_turn(self.agent["id"], "image-turn", operation_id)
                collect.assert_not_called()
        self.assertEqual(self.operations()[0]["phase"], "failed")
        self.assertEqual(self.runtime.agent(self.agent["id"])["checkpointError"], "fixture capture failure")

    def test_successful_image_collection_keeps_exact_result(self):
        operation_id = self.image_operation()
        result = {"collected": True, "rawRef": "fixture-ref"}
        with patch("codex_workspace_images.collect", return_value=result) as collect:
            self.runtime.checkpoint_after_turn(self.agent["id"], "image-turn", operation_id)
            collect.assert_called_once_with(self.agent["id"])
        self.assertEqual(self.operations()[0]["phase"], "completed")
        self.assertEqual(self.runtime.agent(self.agent["id"])["imageWorkspaceCollect"], result)


if __name__ == "__main__":
    unittest.main()
