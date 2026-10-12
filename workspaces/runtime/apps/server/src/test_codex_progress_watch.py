"""Isolated contracts for the active-panel PROGRESS.md watchdog."""

from __future__ import annotations

from server_test_waits import HANG_GUARD_SECONDS

from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
from types import SimpleNamespace
import threading
import unittest
from unittest.mock import patch

from codex_progress import provision_progress, read_progress
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
        self.callback_condition = threading.Condition()
        self.first = provision_progress(self.root, "first")
        self.second = provision_progress(self.root, "second")
        self.first.write_text("First status.\n", encoding="utf-8")
        self.second.write_text("Second status.\n", encoding="utf-8")
        FakeObserver.instances.clear()
        self.patch_observer = patch("codex_progress_watch._observer_class", return_value=FakeObserver)
        self.patch_observer.start()
        self.addCleanup(self.patch_observer.stop)
        self.addCleanup(self.temporary.cleanup)

    def record_callback(self, calls, value):
        with self.callback_condition:
            calls.append(value)
            self.callback_condition.notify_all()

    def new_watcher(self):
        watcher = ProgressFileWatchdog(self.root)
        self.addCleanup(watcher.close)
        return watcher

    def assert_callbacks(self, calls, expected):
        with self.callback_condition:
            self.assertTrue(
                self.callback_condition.wait_for(lambda: len(calls) >= len(expected), timeout=2),
                f"timed out waiting for callbacks: {calls!r}",
            )
            self.assertEqual(calls, expected)

    def track_dispatches(self, watcher):
        completed = threading.Condition()
        count = 0
        changed = watcher._changed

        def tracked_changed(agent_id):
            nonlocal count
            try:
                changed(agent_id)
            finally:
                with completed:
                    count += 1
                    completed.notify_all()

        setattr(watcher, "_changed", tracked_changed)

        def wait_for_dispatches(expected):
            with completed:
                self.assertTrue(
                    completed.wait_for(lambda: count >= expected, timeout=2),
                    f"timed out waiting for {expected} dispatches; saw {count}",
                )

        return wait_for_dispatches

    def test_watches_before_secure_baseline_and_only_signals_new_exact_file_state(self):
        watcher = self.new_watcher()
        calls = []
        original_read = __import__("codex_progress_watch").read_progress

        def observed_read(state_dir, agent_id):
            self.assertTrue(FakeObserver.instances[0].started)
            return original_read(state_dir, agent_id)

        with patch("codex_progress_watch.read_progress", side_effect=observed_read):
            detach = watcher.subscribe("first", lambda: self.record_callback(calls, "changed"))
        observer = FakeObserver.instances[0]
        self.assertEqual(len(observer.watches), 1)
        self.assertEqual(observer.watches[0].path, str(self.first.parent))
        self.assertFalse(observer.watches[0].recursive)

        observer.emit("first", "modified", self.first.parent / "PROGRESS.layout.json")
        observer.emit("first", "modified", self.second)
        observer.emit("first", "modified", self.first.parent / "nested/PROGRESS.md")
        observer.emit("first", "modified", self.first, is_directory=True)
        with self.callback_condition:
            self.assertEqual(calls, [])

        self.first.write_text("Updated status.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first)
        observer.emit("first", "modified", self.first)
        self.assert_callbacks(calls, ["changed"])

        temporary = self.first.with_suffix(".tmp")
        temporary.write_text("Atomic replacement.\n", encoding="utf-8")
        temporary.replace(self.first)
        observer.emit("first", "moved", temporary, dest_path=self.first)
        self.assert_callbacks(calls, ["changed", "changed"])

        self.first.unlink()
        observer.emit("first", "deleted", self.first)
        self.assert_callbacks(calls, ["changed", "changed", "changed"])
        detach()
        detach()
        self.assertEqual(observer.watches, [])
        self.assertFalse(observer.stopped)
        dispatcher = watcher._dispatcher
        watcher.close()
        self.assertTrue(observer.stopped)
        self.assertIsNotNone(dispatcher)
        self.assertFalse(dispatcher.is_alive())
        with self.assertRaisesRegex(RuntimeError, "closed"):
            watcher.subscribe("first", lambda: None)

    def test_parent_directory_change_checks_only_the_exact_progress_revision(self):
        watcher = self.new_watcher()
        wait_for_dispatches = self.track_dispatches(watcher)
        calls = []
        detach = watcher.subscribe("first", lambda: self.record_callback(calls, True))
        self.addCleanup(detach)
        observer = FakeObserver.instances[0]
        observer.emit("first", "modified", self.first.parent, is_directory=True)
        observer.emit("first", "modified", self.first.parent / "nested", is_directory=True)
        wait_for_dispatches(1)
        with self.callback_condition:
            self.assertEqual(calls, [])

        temporary = self.first.with_suffix(".tmp")
        temporary.write_text("Changed through an atomic parent event.\n", encoding="utf-8")
        temporary.replace(self.first)
        observer.emit("first", "modified", self.first.parent, is_directory=True)
        wait_for_dispatches(2)
        self.assert_callbacks(calls, [True])

    def test_in_place_write_can_publish_transient_and_final_revisions(self):
        watcher = self.new_watcher()
        wait_for_dispatches = self.track_dispatches(watcher)
        calls = []
        detach = watcher.subscribe("first", lambda: self.record_callback(calls, True))
        self.addCleanup(detach)
        observer = FakeObserver.instances[0]

        self.first.write_text("", encoding="utf-8")
        observer.emit("first", "modified", self.first.parent, is_directory=True)
        wait_for_dispatches(1)
        self.assert_callbacks(calls, [True])

        self.first.write_text("Completed in-place update.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first.parent, is_directory=True)
        wait_for_dispatches(2)
        self.assert_callbacks(calls, [True, True])

    def test_errors_and_recovery_are_each_change_states_even_with_no_revision(self):
        watcher = self.new_watcher()
        calls = []
        detach = watcher.subscribe("first", lambda: self.record_callback(calls, True))
        observer = FakeObserver.instances[0]

        self.first.write_bytes(b"\xff")
        observer.emit("first", "modified", self.first)
        observer.emit("first", "modified", self.first)
        self.assert_callbacks(calls, [True])

        self.first.write_text("Recovered status.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first)
        self.assert_callbacks(calls, [True, True])
        detach()

    def test_refcounts_share_watch_and_detach_only_after_last_subscriber(self):
        watcher = self.new_watcher()
        first_calls, second_calls = [], []
        detach_first = watcher.subscribe("first", lambda: self.record_callback(first_calls, True))
        detach_second = watcher.subscribe("first", lambda: self.record_callback(second_calls, True))
        observer = FakeObserver.instances[0]
        self.assertEqual(len(observer.watches), 1)

        detach_first()
        self.assertEqual(len(observer.watches), 1)
        self.first.write_text("One subscriber remains.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first)
        with self.callback_condition:
            self.assertEqual(first_calls, [])
        self.assert_callbacks(second_calls, [True])

        detach_second()
        self.assertFalse(observer.stopped)
        self.assertEqual(observer.watches, [])
        detach_second()
        watcher.close()
        self.assertTrue(observer.stopped)

    def test_only_subscribed_agents_receive_changes(self):
        watcher = self.new_watcher()
        first_calls, second_calls = [], []
        detach_first = watcher.subscribe("first", lambda: self.record_callback(first_calls, True))
        detach_second = watcher.subscribe("second", lambda: self.record_callback(second_calls, True))
        observer = FakeObserver.instances[0]
        self.assertEqual(len(observer.watches), 2)

        self.second.write_text("Second changed.\n", encoding="utf-8")
        observer.emit("second", "modified", self.second)
        with self.callback_condition:
            self.assertEqual(first_calls, [])
        self.assert_callbacks(second_calls, [True])

        detach_first()
        self.assertEqual(len(observer.watches), 1)
        detach_second()
        self.assertFalse(observer.stopped)
        self.assertEqual(observer.watches, [])

    def test_native_observer_delivers_a_real_inotify_or_kqueue_change(self):
        # The test suite installs the pinned watchdog dependency in its isolated environment.
        self.patch_observer.stop()
        watcher = self.new_watcher()
        changed = threading.Event()
        detach = watcher.subscribe("first", changed.set)
        try:
            self.first.write_text("Native event.\n", encoding="utf-8")
            self.assertTrue(changed.wait(timeout=HANG_GUARD_SECONDS))
        finally:
            detach()

    def test_native_rapid_detach_keeps_threads_until_close(self):
        # A separate process bounds native observer shutdown and contains threads.
        result = subprocess.run(
            [sys.executable, "-B", "-c", textwrap.dedent("""
                from pathlib import Path
                import tempfile
                import threading
                from unittest.mock import patch
                import codex_progress_watch as module
                from codex_progress import provision_progress

                with tempfile.TemporaryDirectory(prefix="studio-progress-native-race-") as temporary:
                    root = Path(temporary)
                    provision_progress(root, "agent")
                    observer = module._observer_class()()
                    emitter_class = observer._emitter_class
                    original_run = emitter_class.run
                    entered = threading.Event()
                    release = threading.Event()
                    failures = []

                    def paused_run(emitter):
                        entered.set()
                        if not release.wait(3):
                            raise RuntimeError("test did not release native emitter")
                        original_run(emitter)

                    watcher = module.ProgressFileWatchdog(root)
                    with patch.object(emitter_class, "run", paused_run), \
                            patch.object(module, "_observer_class", return_value=lambda: observer):
                        detach = watcher.subscribe("agent", lambda: None)
                        assert entered.wait(2), "native emitter did not enter its run method"
                        emitter = next(iter(observer.emitters))
                        dispatcher = watcher._dispatcher

                        def unsubscribe():
                            try:
                                detach()
                            except BaseException as error:
                                failures.append(error)

                        thread = threading.Thread(target=unsubscribe, daemon=True)
                        thread.start()
                        try:
                            assert emitter.stopped_event.wait(2), "native emitter did not receive stop"
                        finally:
                            release.set()
                        thread.join(3)
                        assert not thread.is_alive(), "native unschedule did not finish after pre-start stop"
                        assert not failures, failures
                        assert not emitter.is_alive(), "native emitter survived pre-start stop"
                        assert observer.is_alive(), "observer stopped on final detach"
                        assert dispatcher is not None and dispatcher.is_alive(), "dispatcher stopped on final detach"
                    watcher.close()
                    assert not observer.is_alive(), "observer survived close"
                    assert dispatcher is not None and not dispatcher.is_alive(), "dispatcher survived close"

                    for _ in range(16):
                        watcher = module.ProgressFileWatchdog(root)
                        detach = watcher.subscribe("agent", lambda: None)
                        observer = watcher._observer
                        emitters = tuple(observer.emitters)
                        dispatcher = watcher._dispatcher
                        detach()
                        detach()
                        assert observer.is_alive(), "rapid detach stopped observer"
                        assert dispatcher is not None and dispatcher.is_alive(), "rapid detach stopped dispatcher"
                        assert watcher._watches == {}, "rapid detach retained native watches"
                        watcher.close()
                        assert not observer.is_alive(), "rapid detach leaked observer"
                        assert all(not emitter.is_alive() for emitter in emitters), "rapid detach leaked emitter"
                        assert dispatcher is None or not dispatcher.is_alive(), "rapid detach leaked dispatcher"
                        assert watcher._watches == {}, "rapid detach retained native watches"
                    print("native persistent observer and 16 rapid detach cycles passed")
            """)],
            cwd=Path(__file__).parent,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("16 rapid detach cycles passed", result.stdout)

    def test_native_watch_handles_agent_without_progress_directory_or_file(self):
        self.patch_observer.stop()
        watcher = self.new_watcher()
        changed = threading.Event()
        pending = self.root / "progress" / "pending-agent" / "PROGRESS.md"
        self.assertFalse(pending.parent.exists())
        self.assertFalse(pending.exists())
        detach = watcher.subscribe("pending-agent", changed.set)
        try:
            self.assertTrue(pending.parent.is_dir())
            self.assertFalse(pending.exists(), "watching must not provision file content")
            self.assertFalse(changed.is_set(), "subscription baseline is not a file change")
            temporary = pending.with_suffix(".tmp")
            temporary.write_text("First progress update.\n", encoding="utf-8")
            temporary.replace(pending)
            self.assertTrue(changed.wait(timeout=HANG_GUARD_SECONDS), "native watch missed first file creation")
            self.assertEqual(
                read_progress(self.root, "pending-agent")["markdown"],
                "First progress update.\n",
            )
        finally:
            detach()

    def test_native_watch_handles_valid_agent_without_progress_directory_or_file(self):
        self.patch_observer.stop()
        watcher = self.new_watcher()
        changed = threading.Event()
        pending = self.root / "progress" / "pending-agent" / "PROGRESS.md"
        self.assertFalse(pending.parent.exists())
        self.assertFalse(pending.exists())
        detach = watcher.subscribe("pending-agent", changed.set)
        try:
            self.assertTrue(pending.parent.is_dir())
            self.assertFalse(pending.exists(), "watching must not provision file content")
            self.assertFalse(changed.is_set(), "subscription baseline is not a file change")
            temporary = pending.with_suffix(".tmp")
            temporary.write_text("First progress update.\n", encoding="utf-8")
            temporary.replace(pending)
            self.assertTrue(changed.wait(timeout=HANG_GUARD_SECONDS), "native watch missed first file creation")
            from codex_progress import read_progress

            self.assertEqual(
                read_progress(self.root, "pending-agent")["markdown"],
                "First progress update.\n",
            )
        finally:
            detach()

    def test_native_dispatch_enqueues_without_waiting_for_watcher_lock(self):
        self.patch_observer.stop()
        watcher = self.new_watcher()
        observer_callback = threading.Event()
        subscribe_started = threading.Event()
        subscribe_finished = threading.Event()
        callback_lock_results = []

        def on_change():
            observer = watcher._observer
            observer_lock_acquired = observer._lock.acquire(timeout=0.5)
            if observer_lock_acquired:
                observer._lock.release()
            # Do not create an observer/condition lock inversion in the probe.
            condition_acquired = watcher._condition.acquire(timeout=0.5)
            callback_lock_results.append((observer_lock_acquired, condition_acquired))
            if condition_acquired:
                watcher._condition.release()
            observer_callback.set()

        detach_first = watcher.subscribe("first", on_change)
        self.addCleanup(detach_first)
        observer = watcher._observer
        handler = watcher._watches["first"][1]
        dispatch = handler.dispatch
        event_dispatched = threading.Event()

        def signal_after_dispatch(event):
            dispatch(event)
            if event.src_path.endswith("PROGRESS.md"):
                event_dispatched.set()

        handler.dispatch = signal_after_dispatch
        detach_second = []
        self.addCleanup(lambda: [detach() for detach in detach_second])

        def subscribe_second():
            subscribe_started.set()
            detach = watcher.subscribe("second", lambda: None)
            detach_second.append(detach)
            subscribe_finished.set()

        watcher._condition.acquire()
        subscribe_thread = threading.Thread(target=subscribe_second)
        try:
            self.first.write_text("Concurrent native update.\n", encoding="utf-8")
            self.assertTrue(event_dispatched.wait(timeout=HANG_GUARD_SECONDS), "native handler waited on watcher lock")
            # BaseObserver owns this lock while dispatching. It must be available
            # even though the watcher condition is still held by this thread.
            self.assertTrue(observer._lock.acquire(timeout=1), "native observer lock remained held")
            observer._lock.release()
            subscribe_thread.start()
            self.assertTrue(subscribe_started.wait(timeout=HANG_GUARD_SECONDS))
            self.assertFalse(subscribe_finished.is_set())
        finally:
            watcher._condition.release()

        subscribe_thread.join(timeout=5)
        try:
            self.assertFalse(subscribe_thread.is_alive(), "second native subscription deadlocked")
            self.assertTrue(subscribe_finished.is_set())
            self.assertTrue(observer_callback.wait(timeout=HANG_GUARD_SECONDS))
            self.assertTrue(callback_lock_results)
            self.assertTrue(all(observer_lock and condition_lock
                                for observer_lock, condition_lock in callback_lock_results))
        finally:
            for detach in detach_second:
                detach()
            detach_first()

    def test_dispatcher_callback_can_detach_last_subscription_and_resubscribe(self):
        self.patch_observer.stop()
        watcher = self.new_watcher()
        replacement_called = threading.Event()
        resubscribed = threading.Event()
        handles = {}

        def replace_subscription():
            handles.pop("first")()
            handles["replacement"] = watcher.subscribe("first", replacement_called.set)
            handles["replacement_observer"] = watcher._observer
            resubscribed.set()

        handles["first"] = watcher.subscribe("first", replace_subscription)
        observer = watcher._observer
        try:
            self.first.write_text("Trigger callback resubscribe.\n", encoding="utf-8")
            self.assertTrue(resubscribed.wait(timeout=HANG_GUARD_SECONDS))
            self.assertIsNotNone(watcher._dispatcher)
            self.assertTrue(watcher._dispatcher.is_alive())

            self.first.write_text("Trigger replacement callback.\n", encoding="utf-8")
            self.assertTrue(replacement_called.wait(timeout=HANG_GUARD_SECONDS))
        finally:
            handles.pop("replacement", lambda: None)()

        self.assertIs(watcher._observer, observer)
        self.assertTrue(observer.is_alive())
        self.assertTrue(watcher._dispatcher.is_alive())
        self.assertEqual(len(watcher._watches), 0)
        self.assertEqual(watcher._subscribers, {})
        watcher.close()
        self.assertFalse(observer.is_alive())
        self.assertFalse(watcher._dispatcher)
        with self.assertRaisesRegex(RuntimeError, "closed"):
            watcher.subscribe("first", lambda: None)

    def test_same_agent_resubscribe_after_in_progress_unschedule_delivers_changes(self):
        watcher = self.new_watcher()
        readded_callback = threading.Event()
        first_detach = watcher.subscribe("first", lambda: None)
        second_detach = watcher.subscribe("second", lambda: None)
        observer = watcher._observer
        first_watch = watcher._watches["first"][0]
        entered_unschedule = threading.Event()
        continue_unschedule = threading.Event()
        schedule_first_again = threading.Event()
        resubscribe_started = threading.Event()
        resubscribe_done = threading.Event()
        replacement_detach = []
        failures = []
        original_unschedule = observer.unschedule
        original_schedule = observer.schedule

        def pause_unschedule(watch):
            if watch is first_watch:
                entered_unschedule.set()
                if not continue_unschedule.wait(timeout=5):
                    raise RuntimeError("test did not release native unschedule")
            return original_unschedule(watch)

        def observe_schedule(event_handler, path, *, recursive=False):
            if path == str(self.first.parent):
                schedule_first_again.set()
            return original_schedule(event_handler, path, recursive=recursive)

        observer.unschedule = pause_unschedule
        observer.schedule = observe_schedule

        def resubscribe():
            resubscribe_started.set()
            try:
                replacement_detach.append(watcher.subscribe("first", readded_callback.set))
            except Exception as error:
                failures.append(error)
            finally:
                resubscribe_done.set()

        detach_thread = threading.Thread(target=first_detach)
        resubscribe_thread = threading.Thread(target=resubscribe)
        def cleanup_threads():
            continue_unschedule.set()
            for thread in (detach_thread, resubscribe_thread):
                if thread.is_alive():
                    thread.join(timeout=5)

        self.addCleanup(cleanup_threads)
        self.addCleanup(continue_unschedule.set)
        self.addCleanup(lambda: [detach() for detach in replacement_detach])
        self.addCleanup(second_detach)

        with patch("codex_progress_watch.WATCHDOG_JOIN_TIMEOUT_SECONDS", 0.05):
            detach_thread.start()
            self.assertTrue(entered_unschedule.wait(timeout=HANG_GUARD_SECONDS))
            resubscribe_thread.start()
            try:
                self.assertTrue(resubscribe_started.wait(timeout=HANG_GUARD_SECONDS))
                # Old code returns a timeout error while the unschedule is
                # deliberately held. Persistent threads leave resubscribe
                # waiting only for the condition lock, then succeed on release.
                resubscribe_done.wait(timeout=0.2)
                self.assertFalse(resubscribe_done.is_set())
            finally:
                continue_unschedule.set()

        detach_thread.join(timeout=HANG_GUARD_SECONDS)
        resubscribe_thread.join(timeout=HANG_GUARD_SECONDS)
        self.assertFalse(detach_thread.is_alive())
        self.assertFalse(resubscribe_thread.is_alive())
        self.assertEqual(failures, [])
        self.assertTrue(resubscribe_done.is_set())
        self.assertTrue(schedule_first_again.is_set())

        self.first.write_text("Fresh watch after detach.\n", encoding="utf-8")
        observer.emit("first", "modified", self.first)
        self.assertTrue(readded_callback.wait(timeout=HANG_GUARD_SECONDS))
        replacement_detach[0]()
        second_detach()
        self.assertEqual(watcher._watches, {})
        self.assertEqual(observer.watches, [])
        self.assertTrue(observer.is_alive())
        watcher.close()
        self.assertFalse(observer.is_alive())
        self.assertFalse(watcher._dispatcher)

    def test_unsupported_platform_does_not_leave_a_subscription(self):
        watcher = self.new_watcher()
        with patch("codex_progress_watch._observer_class", side_effect=RuntimeError("unsupported")):
            with self.assertRaisesRegex(RuntimeError, "unsupported"):
                watcher.subscribe("first", lambda: None)
        self.assertEqual(watcher._watches, {})
        watcher.close()

    def test_observer_start_failure_cleans_up_native_watch(self):
        class FailedObserver(FakeObserver):
            def start(self):
                raise RuntimeError("watch service unavailable")

        watcher = self.new_watcher()
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
