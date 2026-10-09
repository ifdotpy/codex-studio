#!/usr/bin/env python3
"""Checkpoint history format, restore, and lock contracts."""
from codex_layout import REPOSITORY_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import importlib.util
import json
from pathlib import Path
import time
import threading
import unittest
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
spec = importlib.util.spec_from_file_location(
    "workspace_fixture", SERVER_TESTS_ROOT / "workspace-contract.py"
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CheckpointHistoryContract(unittest.TestCase):
    setUp = fixture.WorkspaceContract.setUp
    tearDown = fixture.WorkspaceContract.tearDown
    lead = fixture.WorkspaceContract.lead
    worker = fixture.WorkspaceContract.worker
    git = fixture.WorkspaceContract.git
    git_project = fixture.WorkspaceContract.git_project
    agent_update = fixture.WorkspaceContract.agent_update
    isolated_worker = fixture.WorkspaceContract.isolated_worker

    def add_item(self, agent_id, key, text):
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, agent_id, key, "assistant", text)
            row = db.execute(
                "SELECT record FROM runtime_items WHERE id=?", (agent_id + ":" + key,)
            ).fetchone()
            return json.loads(row[0])

    def test_checkpoint_delta_is_small_and_summary_omits_history(self):
        worker, _ = self.isolated_worker()
        self.agent_update(worker, threadId="history-thread")
        original = [self.add_item(worker["id"], f"old-{i}", "x") for i in range(120)]
        first = self.runtime.checkpoint_capture(worker["id"])
        self.assertNotIn("items", first)
        self.assertEqual(len(first["historyDelta"]), len(original))
        newest = self.add_item(worker["id"], "new", "new")
        second = self.runtime.checkpoint_capture(worker["id"])
        self.assertEqual(second["historyParent"], first["id"])
        self.assertEqual(second["historyDelta"], [newest["id"]])
        self.assertLess(len(json.dumps(second)), 1000)
        old_format_bytes = len(json.dumps({
            **{k: v for k, v in second.items()
               if k not in {"historyParent", "historyDelta", "historyBoundary"}},
            "items": [row["id"] for row in original] + [newest["id"]],
        }))
        self.assertGreater(old_format_bytes, len(json.dumps(second)) * 4)
        print(
            "checkpoint record bytes: "
            f"legacy={old_format_bytes}, lineage={len(json.dumps(second))}"
        )
        summary = self.runtime.workspace_snapshot(worker["id"])["checkpoints"]
        self.assertTrue(summary)
        self.assertTrue(all("items" not in row and "historyDelta" not in row for row in summary))

    def test_restore_resolves_checkpoint_lineage_after_branch(self):
        worker, _ = self.isolated_worker()
        self.agent_update(worker, threadId="history-thread")
        base = self.add_item(worker["id"], "base", "base")
        first = self.runtime.checkpoint_capture(worker["id"], turn_id="base-turn")
        branch_one = self.add_item(worker["id"], "branch-one", "branch one")
        second = self.runtime.checkpoint_capture(worker["id"], turn_id="branch-turn")
        self.assertIn(branch_one["id"], second["historyDelta"])
        preview = self.runtime.checkpoint_preview(worker["id"], first["id"])
        self.runtime.restore_checkpoint(worker["id"], {
            "checkpoint_id": first["id"], "expectedTree": preview["expectedTree"]
        })
        branch_two = self.add_item(worker["id"], "branch-two", "branch two")
        third = self.runtime.checkpoint_capture(worker["id"])
        with self.runtime.db() as db:
            visible = self.runtime._checkpoint_history_ids(db, third)
        self.assertIn(base["id"], visible)
        self.assertIn(branch_two["id"], visible)
        self.assertNotIn(branch_one["id"], visible)
        self.assertEqual(third["historyParent"], first["id"])

    def test_legacy_items_checkpoint_still_restores(self):
        worker, _ = self.isolated_worker()
        self.agent_update(worker, threadId="legacy-thread")
        old = self.add_item(worker["id"], "legacy-old", "old")
        checkpoint = self.runtime.checkpoint_capture(worker["id"], turn_id="legacy-turn")
        later = self.add_item(worker["id"], "legacy-later", "later")
        with self.runtime.lock, self.runtime.db() as db:
            record = next(
                row for row in self.runtime.records(db, "checkpoints")
                if row["id"] == checkpoint["id"]
            )
            record.pop("historyParent", None)
            record.pop("historyDelta", None)
            record.pop("historyBoundary", None)
            record["items"] = [old["id"]]
            self.runtime.put(db, "checkpoints", record)
        preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        self.assertNotIn("items", preview["checkpoint"])
        result = self.runtime.restore_checkpoint(worker["id"], {
            "checkpoint_id": checkpoint["id"], "expectedTree": preview["expectedTree"]
        })
        self.assertEqual(result["status"], "restored")
        with self.runtime.db() as db:
            old_record = json.loads(db.execute(
                "SELECT record FROM runtime_items WHERE id=?", (old["id"],)
            ).fetchone()[0])
            later_record = json.loads(db.execute(
                "SELECT record FROM runtime_items WHERE id=?", (later["id"],)
            ).fetchone()[0])
        self.assertNotIn("afterRestore", old_record)
        self.assertEqual(later_record["afterRestore"], checkpoint["id"])

    def test_git_snapshot_does_not_hold_runtime_lock(self):
        worker, _ = self.isolated_worker()
        self.agent_update(worker, threadId="lock-thread")
        checkpoint = self.runtime.checkpoint_capture(worker["id"], turn_id="lock-turn")
        preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        request = {"checkpoint_id": checkpoint["id"], "expectedTree": preview["expectedTree"]}
        agent = self.runtime.agent(worker["id"])
        started = time.perf_counter()
        with self.runtime.lock:
            self.runtime.snapshot_tree(agent)
            self.runtime.git(
                agent,
                ["diff", "--no-ext-diff", "--no-color", preview["expectedTree"], checkpoint["tree"]],
            )
        before_lock_ms = (time.perf_counter() - started) * 1000
        entered, release = threading.Event(), threading.Event()
        snapshot = self.runtime.snapshot_tree

        def blocked_snapshot(agent):
            self.assertFalse(self.runtime.lock._is_owned())
            entered.set()
            if not release.wait(3):
                raise TimeoutError("test snapshot gate expired")
            return snapshot(agent)

        git = self.runtime.git
        lock_held_git_ms = []

        def unlocked_git(*args, **kwargs):
            held = self.runtime.lock._is_owned()
            self.assertFalse(held)
            git_started = time.perf_counter()
            try:
                return git(*args, **kwargs)
            finally:
                if held:
                    lock_held_git_ms.append((time.perf_counter() - git_started) * 1000)

        with patch.object(self.runtime, "snapshot_tree", side_effect=blocked_snapshot), patch.object(
            self.runtime, "git", side_effect=unlocked_git
        ):
            with concurrent.futures.ThreadPoolExecutor(2) as pool:
                restore = pool.submit(self.runtime.restore_checkpoint, worker["id"], request)
                self.assertTrue(entered.wait(2))
                read_started = time.perf_counter()
                read = pool.submit(self.runtime.agent, worker["id"])
                self.assertEqual(read.result(1)["id"], worker["id"])
                read_ms = (time.perf_counter() - read_started) * 1000
                release.set()
                self.assertEqual(restore.result(5)["status"], "restored")
        self.assertFalse(lock_held_git_ms)
        print(
            "Runtime.lock Git time ms: "
            f"before={before_lock_ms:.1f}, after={sum(lock_held_git_ms):.1f}; "
            f"concurrent read while Git paused={read_ms:.1f}"
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
