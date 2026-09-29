"""Focused local checks for the standalone message-delivery scenario."""
from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from message_delivery import benchmark


class BenchmarkTests(unittest.TestCase):
    def test_nearest_rank_percentiles_and_empty_report(self):
        self.assertEqual(benchmark.percentile([5, 1, 4, 2, 3], .50), 3)
        self.assertEqual(benchmark.percentile([5, 1, 4, 2, 3], .95), 5)
        self.assertIsNone(benchmark.percentile([], .99))
        self.assertEqual(benchmark.summary([2.0]),
                         {"p50": 2.0, "p95": 2.0, "p99": 2.0, "max": 2.0})

    def test_real_http_direct_sse_delivery_and_temporary_state_teardown(self):
        original = tempfile.TemporaryDirectory
        observed = {}
        original_home = os.environ.get("CODEX_HOME")

        class CaptureTemp(original):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, **kwargs)
                observed["name"] = self.name

        with patch.object(benchmark.tempfile, "TemporaryDirectory", CaptureTemp):
            result = benchmark.run_case(1, 3, 80, False, transport="transcript")
        self.assertEqual((result["expected"], result["received"], result["samples"]), (3, 3, 3))
        self.assertEqual(result["transport"], "transcript")
        self.assertEqual(result["queueDrained"], 3)
        self.assertFalse(Path(observed["name"]).exists())
        self.assertEqual(os.environ.get("CODEX_HOME"), original_home)

    def test_primary_sync_stream_and_transcript_pull_delivery(self):
        result = benchmark.run_case(1, 2, 80, False, transport="sync")
        self.assertEqual((result["expected"], result["received"]), (2, 2))
        self.assertEqual(result["transport"], "sync")

    def test_repeated_single_message_import_has_timestamped_receipt_overlap(self):
        results = [benchmark.run_supervised_case(1, 1, 80, True, repetition=index,
                                                  transport="sync")
                   for index in range(1, 4)]
        for result in results:
            progress = result["analyticsProgress"]
            self.assertEqual(progress["importedRecords"], progress["expectedRecords"])
            self.assertEqual(progress["analyticsRowsWritten"], progress["syntheticDataRecords"])
            self.assertTrue(progress["overlappedEventWindow"])
            self.assertIn(progress["overlapWindow"],
                          {"notification_execution", "fixed_offer_to_final_client_receipt"})
            self.assertTrue(progress["overlapWriteTimestampsNs"])
            for stamp in progress["overlapWriteTimestampsNs"]:
                self.assertIn(stamp, progress["writeTimestampsNs"])
                self.assertTrue(any(start <= stamp <= end
                                    for start, end in progress["overlapIntervalsNs"]))

    def test_overlap_windows_use_observed_timestamps(self):
        execution = benchmark.import_overlap([15, 50], [(10, 20)], (0, 100))
        self.assertEqual(execution["overlapWindow"], "notification_execution")
        self.assertEqual(execution["overlapWriteTimestampsNs"], [15])
        fallback = benchmark.import_overlap([50, 150], [(10, 20)], (0, 100))
        self.assertEqual(fallback["overlapWindow"], "fixed_offer_to_final_client_receipt")
        self.assertEqual(fallback["overlapWriteTimestampsNs"], [50])
        self.assertFalse(benchmark.import_overlap([150], [(10, 20)], (0, 100))["overlappedEventWindow"])

    def test_parent_kills_stalled_notification_case_process(self):
        original = tempfile.TemporaryDirectory
        owned = []

        class CaptureTemp(original):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, **kwargs)
                owned.append(self.name)

        with patch.object(benchmark.tempfile, "TemporaryDirectory", CaptureTemp):
            with self.assertRaisesRegex(TimeoutError, "child terminated"):
                benchmark.run_supervised_case(1, 1, 80, False, transport="transcript",
                                              inject="stall_notification", timeout=1)
        self.assertTrue(owned)
        self.assertTrue(all(not Path(name).exists() for name in owned))

    def test_missing_event_and_corrupt_final_text_fail(self):
        with self.assertRaisesRegex(RuntimeError, "offered 2 messages but queued 1"):
            benchmark.run_case(1, 2, 80, False, inject="missing", transport="transcript")
        with self.assertRaisesRegex(RuntimeError, "corrupt final text"):
            benchmark.run_case(1, 2, 80, False, inject="corrupt", transport="transcript")

    def test_http_timeout_fails_promptly(self):
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        accepted = threading.Event()
        connection = []

        def accept_and_hold():
            conn, _ = listener.accept()
            connection.append(conn)
            accepted.set()
            conn.recv(4096)

        holder = threading.Thread(target=accept_and_hold, daemon=True)
        holder.start()
        errors = queue.Queue()
        client = benchmark.SSEClient(f"http://127.0.0.1:{listener.getsockname()[1]}/stream",
                                     "timeout", {}, {}, errors, threading.Event(), timeout=.05)
        client.start()
        self.assertTrue(accepted.wait(1))
        client.join(1)
        listener.close()
        for conn in connection:
            conn.close()
        holder.join(1)
        self.assertFalse(client.is_alive())
        self.assertIn("timed out", errors.get_nowait().lower())

    def test_native_transport_request_is_hard_failure(self):
        with patch.dict("os.environ", {"CODEX_BENCH_NATIVE_TRANSPORT": "1"}):
            with self.assertRaisesRegex(RuntimeError, "native transport is forbidden"):
                benchmark.run_case(1, 1, 80, False, transport="transcript")

    def test_dispatch_exception_fails_cli_without_hanging(self):
        script = Path(benchmark.__file__).resolve()
        environment = {**os.environ, "CODEX_BENCH_TEST_DISPATCH_FAILURE": "1"}
        result = subprocess.run([sys.executable, str(script), "--check"], env=environment,
                                capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("injected benchmark dispatch failure", result.stderr)

    def test_cli_rejects_unbounded_or_empty_configuration(self):
        script = Path(benchmark.__file__).resolve()
        for option in (("--rate", "nan"), ("--rate", "inf"), ("--repetitions", "0")):
            result = subprocess.run([sys.executable, str(script), *option],
                                    capture_output=True, text=True, timeout=2)
            self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
