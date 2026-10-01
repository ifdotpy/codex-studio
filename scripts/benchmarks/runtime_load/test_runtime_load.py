"""Small deterministic checks for the runtime load report helpers."""
import importlib.util
from pathlib import Path
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


if __name__ == "__main__":
    unittest.main()
