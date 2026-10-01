"""Small deterministic checks for the runtime load report helpers."""
import importlib.util
from contextlib import contextmanager
from pathlib import Path
import sqlite3
import threading
import time
import unittest

path = Path(__file__).with_name("server.py")
spec = importlib.util.spec_from_file_location("runtime_load_server", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class RuntimeLoadHelpersTests(unittest.TestCase):
    def test_nearest_rank_latency_statistics(self):
        self.assertEqual(module.stats([4, 1, 3, 2]),
                         {"samples": 4, "p50": 2, "p95": 4, "p99": 4, "max": 4})

    def test_empty_latency_series_is_explicit(self):
        self.assertEqual(module.stats([]),
                         {"samples": 0, "p50": None, "p95": None, "p99": None, "max": None})

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
        self.assertGreater(metrics["waitMsByContext"][thread.name]["samples"], 0)
        self.assertGreater(metrics["heldMsByContext"][thread.name]["samples"], 0)
        progress = lock.progress_snapshot()
        self.assertGreater(progress["waitByContext"][thread.name]["samples"], 0)
        self.assertGreaterEqual(progress["waitByContext"][thread.name]["maxMs"],
                                progress["waitByContext"][thread.name]["latestMs"])


if __name__ == "__main__":
    unittest.main()
