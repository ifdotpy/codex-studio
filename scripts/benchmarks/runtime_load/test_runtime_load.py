"""Small deterministic checks for the runtime load report helpers."""
import importlib.util
from contextlib import contextmanager
from pathlib import Path
import sqlite3
import sys
import threading
import tempfile
import time
import unittest
from unittest import mock

path = Path(__file__).with_name("server.py")
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
spec = importlib.util.spec_from_file_location("runtime_load_server", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class RuntimeLoadHelpersTests(unittest.TestCase):
    def test_runtime_fixture_sets_synchronous_mode_on_each_connection(self):
        from codex_runtime import Runtime

        class FixtureRuntime(module.BenchmarkRuntimeMixin, Runtime):
            def schedule(self):
                return

        with tempfile.TemporaryDirectory() as directory:
            runtime = object.__new__(FixtureRuntime)
            runtime.db_path = Path(directory) / "production-runtime.sqlite3"
            runtime.analytics_db_path = Path(directory) / "analytics.sqlite3"
            runtime.changed = threading.Event()
            for mode, expected in (("FULL", 2), ("NORMAL", 1)):
                with self.subTest(mode=mode), mock.patch.dict(
                        "os.environ", {"BENCH_SQLITE_SYNCHRONOUS": mode}):
                    for _ in range(2):
                        with runtime.db() as db:
                            self.assertEqual(
                                db.execute("PRAGMA synchronous").fetchone()[0], expected)

    def test_diagnostic_synchronous_rejects_unsupported_modes(self):
        for mode in ("OFF", "EXTRA", "2"):
            with self.subTest(mode=mode), mock.patch.dict(
                    "os.environ", {"BENCH_SQLITE_SYNCHRONOUS": mode}):
                with self.assertRaisesRegex(ValueError, "must be FULL or NORMAL"):
                    module.diagnostic_synchronous_mode()

    def test_idle_connection_diagnostic_is_opt_in_and_validated(self):
        for setting, expected in (("0", False), ("1", True)):
            with self.subTest(setting=setting), mock.patch.dict(
                    "os.environ", {"BENCH_SQLITE_IDLE_CONNECTION": setting}):
                self.assertEqual(module.idle_connection_diagnostic_enabled(), expected)
        with mock.patch.dict("os.environ", {"BENCH_SQLITE_IDLE_CONNECTION": "true"}):
            with self.assertRaisesRegex(ValueError, "must be 0 or 1"):
                module.idle_connection_diagnostic_enabled()

    def test_idle_connection_read_primes_wal_then_stays_idle_until_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "idle.sqlite3"
            setup = sqlite3.connect(path)
            self.assertEqual(setup.execute("PRAGMA journal_mode=WAL").fetchone()[0].lower(), "wal")
            setup.execute("CREATE TABLE sample(value INTEGER)")
            setup.commit()
            setup.close()

            connection = module.open_idle_connection(path)
            try:
                self.assertFalse(connection.in_transaction)
                writer = sqlite3.connect(path)
                try:
                    writer.execute("INSERT INTO sample VALUES (1)")
                    writer.commit()
                finally:
                    writer.close()
                self.assertFalse(connection.in_transaction)
                self.assertTrue(Path(f"{path}-wal").exists())
            finally:
                connection.close()
            self.assertFalse(Path(f"{path}-wal").exists())

    def test_nearest_rank_latency_statistics(self):
        self.assertEqual(module.stats([4, 1, 3, 2]),
                         {"samples": 4, "p50": 2, "p95": 4, "p99": 4, "max": 4})

    def test_timing_statistics_include_sample_count_sum_and_mean(self):
        self.assertEqual(module.timing_stats([1.0, 3.0]),
                         {"samples": 2, "p50": 1.0, "p95": 3.0, "p99": 3.0,
                          "max": 3.0, "percentileWindowSamples": 2,
                          "totalMs": 4.0, "meanMs": 2.0})

    def test_timing_statistics_keep_cumulative_totals_separate_from_percentile_window(self):
        self.assertEqual(module.timing_stats([2.0, 3.0], 4, 10.0),
                         {"samples": 4, "p50": 2.0, "p95": 3.0, "p99": 3.0,
                          "max": 3.0, "percentileWindowSamples": 2,
                          "totalMs": 10.0, "meanMs": 2.5})

    def test_empty_latency_series_is_explicit(self):
        self.assertEqual(module.stats([]),
                         {"samples": 0, "p50": None, "p95": None, "p99": None, "max": None})

    def test_progress_keeps_intents_offers_and_completions_separate(self):
        phase = "steady"
        progress = module.phase_progress_turn_counts(
            phase, 2.0,
            {"warmup": 1, "steady": 3, "burst": 0, "drain": 0},
            {"warmup": [1], "steady": [10, 20], "burst": [], "drain": []},
            {"warmup": 1, "steady": 1, "burst": 0, "drain": 0},
        )
        self.assertEqual(progress["scheduledIntentsByPhase"][phase], 3)
        self.assertEqual(progress["phaseTurnsStartedOffered"][phase], 2)
        self.assertEqual(progress["phaseTurnsCompleted"][phase], 1)
        self.assertEqual(progress["currentPhaseAchievedOfferedTurnsPerSecond"], 1)
        self.assertEqual(progress["currentPhaseAchievedCompletedTurnsPerSecond"], .5)

    def test_callback_duration_is_recorded_when_production_callback_raises(self):
        durations = []

        def fail():
            raise ValueError("synthetic callback failure")

        with self.assertRaisesRegex(ValueError, "synthetic callback failure"):
            module.invoke_with_duration(fail, durations.append)
        self.assertEqual(len(durations), 1)
        self.assertGreaterEqual(durations[0], 0)

    def test_phase_schedule_restarts_for_each_rate(self):
        start = 10_000_000_000
        self.assertEqual(module.phase_due_ns(start, 3, 2), start + 1_500_000_000)
        next_phase = start + 4_000_000_000
        self.assertEqual(module.phase_due_ns(next_phase, 0, 8), next_phase)
        self.assertEqual(module.phase_due_ns(next_phase, 1, 8) - next_phase,
                         125_000_000)

    def test_sustained_phase_offers_configured_duration_without_skipping_workers(self):
        counts = module.phase_turn_counts(256, 1, 160, 30)
        self.assertEqual(counts, {
            "warmup": 256,
            "steady": 4800,
            "burst": 256,
            "drain": 256,
        })

    def test_steady_minimum_covers_every_worker_when_duration_is_short(self):
        counts = module.phase_turn_counts(256, 1, 1, 1)
        self.assertEqual(counts["steady"], 256)

    def test_stream_identity_uses_only_real_protocol_fields(self):
        base = {"threadId": "t", "turnId": "turn", "itemId": "item",
                "delta": "chunk", "_benchEventId": "ignored"}
        self.assertEqual(module.delta_identity(base), ("t", "turn", "item", "chunk"))

    def test_duplicate_stream_fragments_keep_each_offered_identity(self):
        fragments = {}
        params = {"threadId": "t", "turnId": "turn", "itemId": "item",
                  "delta": "repeated"}
        module.register_delta_identity(fragments, params, ("event-1", "assistantDelta"))
        module.register_delta_identity(fragments, params, ("event-2", "assistantDelta"))
        self.assertEqual(module.take_delta_identity(fragments, params),
                         ("event-1", "assistantDelta"))
        self.assertEqual(module.take_delta_identity(fragments, params),
                         ("event-2", "assistantDelta"))
        self.assertEqual(fragments, {})

    def test_witness_marker_is_unique_per_phase_and_round(self):
        marks = {module.witness_marker("abcdefgh-0", phase, round_index)
                 for phase in ("warmup", "steady", "burst", "drain")
                 for round_index in range(2)}
        self.assertEqual(len(marks), 8)

    def test_final_render_witness_is_absent_from_stream_deltas(self):
        marker = module.witness_marker("abcdefgh-0", "drain", 0)
        stream_text, final_text = module.assistant_witness_text(
            "drain", marker, "Load 01/01", 0
        )
        self.assertNotIn(marker, stream_text)
        self.assertIn(marker, final_text)

    def test_delivery_receipt_preserves_exact_runtime_event_id(self):
        receipt = module.delivery_receipt("event-17", "thread-3", "turn-9")
        self.assertEqual(receipt["params"]["item"]["clientId"], "event-17")
        self.assertEqual(receipt["params"]["threadId"], "thread-3")
        self.assertEqual(receipt["params"]["turnId"], "turn-9")

    def test_schema_accounting_retries_only_exact_sqlite_schema_code(self):
        schema = sqlite3.OperationalError("synthetic schema race")
        schema.sqlite_errorcode = sqlite3.SQLITE_SCHEMA
        busy = sqlite3.OperationalError("database is locked")
        busy.sqlite_errorcode = sqlite3.SQLITE_BUSY
        self.assertTrue(module.is_sqlite_schema_error(schema))
        self.assertFalse(module.is_sqlite_schema_error(busy))
        self.assertFalse(module.is_sqlite_schema_error(RuntimeError("schema changed")))

    def test_read_only_accounting_uses_fresh_connection_and_records_recovery(self):
        opened = []

        class Connection:
            def __init__(self, number):
                self.number = number

            def execute(self, sql):
                if sql == "PRAGMA schema_version":
                    return type("Row", (), {"fetchone": lambda _self: (self.number,)})()
                if self.number == 1:
                    error = sqlite3.OperationalError("database schema has changed")
                    error.sqlite_errorcode = sqlite3.SQLITE_SCHEMA
                    raise error
                return type("Row", (), {"fetchone": lambda _self: ("ok",)})()

        @contextmanager
        def db_factory():
            connection = Connection(len(opened) + 1)
            opened.append(connection)
            yield connection

        records = []
        value = module.read_accounting_snapshot(
            db_factory, "fixture.count", lambda db: db.execute("SELECT count").fetchone()[0], records)
        self.assertEqual(value, "ok")
        self.assertGreaterEqual(len(opened), 3)  # failed query, fresh schema read, fresh retry
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["sqlIdentity"], "fixture.count")
        self.assertEqual(records[0]["errorName"], "OperationalError")
        self.assertEqual(records[0]["sqliteErrorCode"], sqlite3.SQLITE_SCHEMA)
        self.assertEqual(records[0]["schemaVersionBefore"], 1)
        self.assertEqual(records[0]["schemaVersionAfterError"], 2)
        self.assertEqual(records[0]["schemaVersionAfter"], 3)
        self.assertTrue(records[0]["recovered"])
        self.assertGreaterEqual(records[0]["durationMs"], 0)

    def test_read_only_accounting_does_not_retry_other_sqlite_errors(self):
        opened = []

        class Connection:
            def execute(self, sql):
                if sql == "PRAGMA schema_version":
                    return type("Row", (), {"fetchone": lambda _self: (1,)})()
                error = sqlite3.OperationalError("database is locked")
                error.sqlite_errorcode = sqlite3.SQLITE_BUSY
                raise error

        @contextmanager
        def db_factory():
            opened.append(object())
            yield Connection()

        with self.assertRaises(sqlite3.OperationalError):
            module.read_accounting_snapshot(
                db_factory, "fixture.locked", lambda db: db.execute("SELECT count"), [])
        self.assertEqual(len(opened), 1)

    def test_read_only_accounting_fails_after_three_schema_errors(self):
        opened = []

        class Connection:
            def execute(self, sql):
                if sql == "PRAGMA schema_version":
                    return type("Row", (), {"fetchone": lambda _self: (1,)})()
                error = sqlite3.OperationalError("database schema has changed")
                error.sqlite_errorcode = sqlite3.SQLITE_SCHEMA
                raise error

        @contextmanager
        def db_factory():
            opened.append(object())
            yield Connection()

        records = []
        with self.assertRaises(sqlite3.OperationalError):
            module.read_accounting_snapshot(
                db_factory, "fixture.schema-race", lambda db: db.execute("SELECT count"), records)
        self.assertEqual(len(records), 3)
        self.assertEqual(sum(1 for _ in opened), 6)  # query and fresh schema read per attempt
        self.assertTrue(all(record["errorName"] == "OperationalError" for record in records))
        self.assertTrue(all(record["sqlIdentity"] == "fixture.schema-race" for record in records))

    def test_read_only_accounting_fails_if_retry_exceeds_total_time_bound(self):
        calls = [0]

        class Connection:
            def execute(self, sql):
                if sql == "PRAGMA schema_version":
                    return type("Row", (), {"fetchone": lambda _self: (1,)})()
                calls[0] += 1
                if calls[0] == 1:
                    error = sqlite3.OperationalError("database schema has changed")
                    error.sqlite_errorcode = sqlite3.SQLITE_SCHEMA
                    raise error
                time.sleep(0.02)
                return type("Row", (), {"fetchone": lambda _self: ("late",)})()

        @contextmanager
        def db_factory():
            yield Connection()

        records = []
        with self.assertRaisesRegex(RuntimeError, "exceeded its total time bound"):
            module.read_accounting_snapshot(
                db_factory, "fixture.slow-retry",
                lambda db: db.execute("SELECT count").fetchone()[0], records,
                max_elapsed_seconds=0.005)
        self.assertEqual(records[0]["recovered"], False)
        self.assertTrue(records[0]["retryBoundExceeded"])
        self.assertGreater(records[0]["totalDurationMs"], 5)

    def test_measured_rlock_preserves_reentrancy_and_condition_wait(self):
        lock = module.MeasuredRLock()
        condition = threading.Condition(lock)
        notified = threading.Event()

        def notify():
            time.sleep(0.01)
            with condition:
                condition.notify()
                notified.set()

        thread = threading.Thread(target=notify)
        thread.start()
        with condition:
            with lock:
                self.assertTrue(condition.wait(timeout=1))
        thread.join(timeout=1)
        self.assertTrue(notified.is_set())
        metrics = lock.snapshot()
        self.assertGreater(metrics["waitMsByContext"]["other"]["samples"], 0)
        self.assertGreater(metrics["heldMsByContext"]["other"]["samples"], 0)
        progress = lock.progress_snapshot()
        self.assertGreater(progress["waitByContext"]["other"]["samples"], 0)
        self.assertGreaterEqual(progress["waitByContext"]["other"]["maxMs"],
                                progress["waitByContext"]["other"]["latestMs"])

    def test_measured_plain_lock_counts_nonblocking_failures(self):
        lock = module.MeasuredLock(threading.Lock())
        self.assertTrue(lock.acquire())
        self.assertFalse(lock.acquire(blocking=False))
        snapshot = lock.progress_snapshot()
        self.assertEqual(snapshot["failedNonblockingAcquires"], 1)
        self.assertEqual(snapshot["acquires"], 1)
        lock.release()

    def test_producer_pool_threads_keep_producer_lock_attribution(self):
        result = []

        def classify():
            result.append(module.MeasuredRLock._context())

        thread = threading.Thread(target=classify, name="bench-producer_0")
        thread.start()
        thread.join(timeout=1)
        self.assertEqual(result, ["producer"])

    def test_measured_runtime_db_context_delegates_fake_sampled_unsampled_errors_and_nesting(self):
        lifecycle = []
        records = []

        class FakeContext:
            def __init__(self, identity, suppress=False):
                self.identity = identity
                self.suppress = suppress

            def __enter__(self):
                lifecycle.append(("enter", self.identity))
                return self.identity

            def __exit__(self, error_type, error, traceback):
                lifecycle.append(("exit", self.identity, error_type, error))
                return self.suppress

        contexts = []

        def original_runtime_db():
            context = FakeContext(len(contexts))
            contexts.append(context)
            return context

        with module.measured_runtime_db_context(
                original_runtime_db, True,
                lambda metric, elapsed: records.append((metric, elapsed))) as outer:
            self.assertEqual(outer, 0)
            with module.measured_runtime_db_context(
                    original_runtime_db, False,
                    lambda *_: self.fail("unsampled context must not record timings")) as inner:
                self.assertEqual(inner, 1)

        self.assertEqual([item[0] for item in lifecycle], ["enter", "enter", "exit", "exit"])
        self.assertEqual([item[1] for item in lifecycle], [0, 1, 1, 0])
        self.assertEqual([metric for metric, _ in records],
                         ["contextEntryMs", "contextBodyMs", "contextExitAndCloseMs"])
        self.assertTrue(all(elapsed >= 0 for _, elapsed in records))

        marker = ValueError("original error")
        with self.assertRaisesRegex(ValueError, "original error"):
            with module.measured_runtime_db_context(
                    original_runtime_db, True,
                    lambda metric, elapsed: records.append((metric, elapsed))):
                raise marker
        self.assertIs(lifecycle[-1][2], ValueError)
        self.assertIs(lifecycle[-1][3], marker)

        unsampled_error_records = []
        with self.assertRaises(ValueError) as caught:
            with module.measured_runtime_db_context(
                    original_runtime_db, False,
                    lambda metric, elapsed: unsampled_error_records.append((metric, elapsed))):
                raise marker
        self.assertIs(caught.exception, marker)
        self.assertEqual(unsampled_error_records, [])
        self.assertIs(lifecycle[-1][3], marker)

        contexts.append(FakeContext("suppress", suppress=True))
        suppressing_factory = lambda: contexts[-1]
        with module.measured_runtime_db_context(suppressing_factory, True, lambda *_: None):
            raise RuntimeError("suppressed by original context")
        self.assertEqual(lifecycle[-1][1], "suppress")
        self.assertIs(lifecycle[-1][2], RuntimeError)

    def test_measured_runtime_db_context_uses_real_runtime_commit_rollback_and_close(self):
        from codex_runtime import Runtime

        class FixtureRuntime(module.BenchmarkRuntimeMixin, Runtime):
            pass

        with tempfile.TemporaryDirectory() as directory:
            runtime = object.__new__(FixtureRuntime)
            runtime.db_path = Path(directory) / "production-runtime.sqlite3"
            runtime.analytics_db_path = Path(directory) / "analytics.sqlite3"
            runtime.changed = threading.Event()
            records = []
            original_runtime_db = runtime.db

            # Sampled use yields the real production connection and records only
            # context boundaries; it does not wrap or replace SQLite methods.
            with mock.patch.dict("os.environ", {"BENCH_SQLITE_SYNCHRONOUS": "FULL"}):
                with module.measured_runtime_db_context(
                        original_runtime_db, True,
                        lambda metric, elapsed: records.append((metric, elapsed))) as connection:
                    self.assertIsInstance(connection, sqlite3.Connection)
                    yielded_connection = connection
                    connection.execute("CREATE TABLE sample(value INTEGER)")
                    connection.execute("INSERT INTO sample VALUES (7)")
            self.assertIs(connection, yielded_connection)
            self.assertEqual([metric for metric, _ in records],
                             ["contextEntryMs", "contextBodyMs", "contextExitAndCloseMs"])
            self.assertTrue(all(elapsed >= 0 for _, elapsed in records))
            with self.assertRaises(sqlite3.ProgrammingError):
                yielded_connection.execute("SELECT 1")

            # Unsampled nesting still enters each original Runtime.db manager;
            # inner commit is visible to the enclosing connection.
            with mock.patch.dict("os.environ", {"BENCH_SQLITE_SYNCHRONOUS": "FULL"}):
                with module.measured_runtime_db_context(
                        original_runtime_db, True,
                        lambda metric, elapsed: records.append((metric, elapsed))) as outer:
                    outer.execute("SELECT value FROM sample").fetchall()
                    with module.measured_runtime_db_context(
                            original_runtime_db, False,
                            lambda *_: self.fail("unsampled context must not record timings")) as inner:
                        inner_connection = inner
                        inner.execute("INSERT INTO sample VALUES (9)")
                    self.assertEqual(
                        [tuple(row) for row in outer.execute(
                            "SELECT value FROM sample ORDER BY value").fetchall()],
                        [(7,), (9,)])
            with self.assertRaises(sqlite3.ProgrammingError):
                inner_connection.execute("SELECT 1")

            # The original Runtime.db manager owns rollback and re-raises the
            # same body error; the instrumentation does not replay the write.
            failed_connection = None
            with self.assertRaisesRegex(ValueError, "rollback fixture"):
                with module.measured_runtime_db_context(
                        original_runtime_db, True,
                        lambda metric, elapsed: records.append((metric, elapsed))) as connection:
                    failed_connection = connection
                    connection.execute("INSERT INTO sample VALUES (8)")
                    raise ValueError("rollback fixture")
            rollback_records = records[-3:]
            self.assertEqual([metric for metric, _ in rollback_records],
                             ["contextEntryMs", "contextBodyMs", "contextExitAndCloseMs"])
            with self.assertRaises(sqlite3.ProgrammingError):
                failed_connection.execute("SELECT 1")
            with runtime.db() as verification:
                self.assertEqual(
                    [tuple(row) for row in verification.execute(
                        "SELECT value FROM sample ORDER BY value").fetchall()],
                    [(7,), (9,)])


if __name__ == "__main__":
    unittest.main()
