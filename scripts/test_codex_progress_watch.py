"""Isolated contracts for the active-panel PROGRESS.md watchdog."""

from __future__ import annotations

from pathlib import Path
import tempfile
from types import SimpleNamespace
import threading
import unittest
from unittest.mock import patch

from codex_progress import provision_progress
from codex_progress_watch import ProgressFileWatchdog


class FakeObserver:
    instances: list[FakeObserver] = []

    def __init__(self):
        self.watches = []
        self.started = False
        self.stopped = False
        self.unscheduled = []
        self.instances.append(self)

    def schedule(self, handler, path, recursive=False):
        watch = SimpleNamespace(handler=handler, path=path, recursive=recursive)
        self.watches.append(watch)
        return watch

    def start(self):
        self.started = True

    def unschedule(self, watch):
        self.unscheduled.append(watch)
        self.watches.remove(watch)

    def stop(self):
        self.stopped = True

    def join(self, timeout):
        self.join_timeout = timeout

    def is_alive(self):
        return self.started and not self.stopped

    def emit(self, agent_id, event_type, src_path, *, dest_path="", is_directory=False):
        watch = next(watch for watch in self.watches if watch.handler._agent_id == agent_id)
        watch.handler.dispatch(
            SimpleNamespace(
                event_type=event_type,
                src_path=str(src_path),
                dest_path=str(dest_path),
                is_directory=is_directory,
            )
        )


class ProgressWatchdogTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="studio-progress-watch-")
        self.root = Path(self.temporary.name)
        self.first = provision_progress(self.root, "first")
        self.second = provision_progress(self.root, "second")
        self.first.write_text("First status.\n", encoding="utf-8")
        self.second.write_text("Second status.\n", encoding="utf-8")
        FakeObserver.instances.clear()
        self.patch_observer = patch("codex_progress_watch._observer_class", return_value=FakeObserver)
        self.patch_observer.start()
        self.addCleanup(self.patch_observer.stop)
        self.addCleanup(self.temporary.cleanup)

    def test_watches_before_secure_baseline_and_only_signals_new_exact_file_state(self):
        watcher = ProgressFileWatchdog(self.root)
        calls = []
        original_read = __import__("codex_progress_watch").read_progress

        def observed_read(state_dir, agent_id):
            self.assertTrue(FakeObserver.instances[0].started)
            return original_read(state_dir, agent_id)

        with patch("codex_progress_watch.read_progress", side_effect=observed_read):
            detach = watcher.subscribe("first", lambda: calls.append("changed"))
        observer = FakeObserver.instances[0]
        self.assertEqual(len(observer.watches), 1)
        self.assertEqual(observer.watches[0].path, str(self.first.parent))
        self.assertFalse(observer.watches[0].recursive)

        observer.emit("first", "modified", self.first.parent / "PROGRESS.layout.json")
        observer.emit("first", "modified", self.second)
        observer.emit("first", "modified", self.first.parent / "nested/PROGRESS.md")
        observer.emit("first", "modified", self.first, is_directory=True)
        self.assertEqual(calls, [])

        self.first.write_text("Updated status.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first)
        observer.emit("first", "modified", self.first)
        self.assertEqual(calls, ["changed"])

        temporary = self.first.with_suffix(".tmp")
        temporary.write_text("Atomic replacement.\n", encoding="utf-8")
        temporary.replace(self.first)
        observer.emit("first", "moved", temporary, dest_path=self.first)
        self.assertEqual(calls, ["changed", "changed"])

        self.first.unlink()
        observer.emit("first", "deleted", self.first)
        self.assertEqual(calls, ["changed", "changed", "changed"])
        detach()
        detach()
        self.assertEqual(observer.watches, [])
        self.assertTrue(observer.stopped)

    def test_errors_and_recovery_are_each_change_states_even_with_no_revision(self):
        watcher = ProgressFileWatchdog(self.root)
        calls = []
        detach = watcher.subscribe("first", lambda: calls.append(True))
        observer = FakeObserver.instances[0]

        self.first.write_bytes(b"\xff")
        observer.emit("first", "modified", self.first)
        observer.emit("first", "modified", self.first)
        self.assertEqual(calls, [True])

        self.first.write_text("Recovered status.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first)
        self.assertEqual(calls, [True, True])
        detach()

    def test_refcounts_share_watch_and_detach_only_after_last_subscriber(self):
        watcher = ProgressFileWatchdog(self.root)
        first_calls, second_calls = [], []
        detach_first = watcher.subscribe("first", lambda: first_calls.append(True))
        detach_second = watcher.subscribe("first", lambda: second_calls.append(True))
        observer = FakeObserver.instances[0]
        self.assertEqual(len(observer.watches), 1)

        detach_first()
        self.assertEqual(len(observer.watches), 1)
        self.first.write_text("One subscriber remains.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first)
        self.assertEqual(first_calls, [])
        self.assertEqual(second_calls, [True])

        detach_second()
        self.assertTrue(observer.stopped)
        detach_second()
        watcher.close()

    def test_only_subscribed_agents_receive_changes(self):
        watcher = ProgressFileWatchdog(self.root)
        first_calls, second_calls = [], []
        detach_first = watcher.subscribe("first", lambda: first_calls.append(True))
        detach_second = watcher.subscribe("second", lambda: second_calls.append(True))
        observer = FakeObserver.instances[0]
        self.assertEqual(len(observer.watches), 2)

        self.second.write_text("Second changed.\n", encoding="utf-8")
        observer.emit("second", "modified", self.second)
        self.assertEqual(first_calls, [])
        self.assertEqual(second_calls, [True])

        detach_first()
        self.assertEqual(len(observer.watches), 1)
        detach_second()
        self.assertTrue(observer.stopped)

    def test_native_observer_delivers_a_real_inotify_or_fsevents_change(self):
        # The test suite installs the pinned watchdog dependency in its isolated environment.
        self.patch_observer.stop()
        watcher = ProgressFileWatchdog(self.root)
        changed = threading.Event()
        detach = watcher.subscribe("first", changed.set)
        try:
            self.first.write_text("Native event.\n", encoding="utf-8")
            self.assertTrue(changed.wait(timeout=5))
        finally:
            detach()

    def test_unsupported_platform_does_not_leave_a_subscription(self):
        watcher = ProgressFileWatchdog(self.root)
        with patch("codex_progress_watch._observer_class", side_effect=RuntimeError("unsupported")):
            with self.assertRaisesRegex(RuntimeError, "unsupported"):
                watcher.subscribe("first", lambda: None)
        self.assertEqual(watcher._watches, {})
        watcher.close()

    def test_observer_start_failure_cleans_up_native_watch(self):
        class FailedObserver(FakeObserver):
            def start(self):
                raise RuntimeError("watch service unavailable")

        watcher = ProgressFileWatchdog(self.root)
        with patch("codex_progress_watch._observer_class", return_value=FailedObserver):
            with self.assertRaisesRegex(RuntimeError, "watch service unavailable"):
                watcher.subscribe("first", lambda: None)
        observer = FailedObserver.instances[-1]
        self.assertEqual(observer.watches, [])
        self.assertEqual(len(observer.unscheduled), 1)
        self.assertFalse(observer.stopped)
        watcher.close()


if __name__ == "__main__":
    unittest.main()
