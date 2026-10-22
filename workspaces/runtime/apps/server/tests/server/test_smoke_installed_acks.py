"""The installed-build smoke fails the acknowledgement check only on a stall."""

import importlib.util
from pathlib import Path
import unittest

SMOKE_PATH = Path(__file__).resolve().parents[2] / "src" / "smoke-installed.py"
SMOKE_SPEC = importlib.util.spec_from_file_location("smoke_installed", SMOKE_PATH)
SMOKE = importlib.util.module_from_spec(SMOKE_SPEC)
SMOKE_SPEC.loader.exec_module(SMOKE)


def handle(sequence, acknowledged):
    return {"id": "account:default", "sequence": sequence, "acknowledged": acknowledged}


class FakeSupervisor:
    """Returns recorded samples in order and keeps a fake clock; it never sleeps."""

    def __init__(self, samples):
        self.samples = list(samples)
        self.now = 0.0
        self.reads = 0

    def sample(self):
        self.reads += 1
        reading = self.samples[min(self.reads, len(self.samples)) - 1]
        return {"account:default": reading} if reading is not None else {}

    def sleep(self, seconds):
        self.now += seconds

    def stalls(self, first):
        return SMOKE.acknowledgement_stalls(
            {"account:default": first}, self.sample, window=20.0, poll=1.0,
            sleep=self.sleep, clock=lambda: self.now)


class AcknowledgementStalls(unittest.TestCase):
    def test_lag_under_load_that_progresses_passes(self):
        # The reported case: 13 pending; later samples still lag, but the cursor moved.
        supervisor = FakeSupervisor([handle(385984, 385978), handle(385996, 385996)])
        stalled, latest = supervisor.stalls(handle(385978, 385965))
        self.assertEqual(stalled, [])
        self.assertEqual(supervisor.reads, 1)
        self.assertEqual(latest["account:default"]["acknowledged"], 385978)

    def test_late_progress_inside_the_window_passes(self):
        supervisor = FakeSupervisor([handle(110, 100)] * 9 + [handle(112, 101)])
        stalled, _latest = supervisor.stalls(handle(110, 100))
        self.assertEqual(stalled, [])
        self.assertEqual(supervisor.reads, 10)

    def test_stuck_cursor_with_pending_events_fails(self):
        supervisor = FakeSupervisor([handle(110, 100), handle(130, 100)])
        stalled, _latest = supervisor.stalls(handle(105, 100))
        self.assertEqual(len(stalled), 1)
        self.assertIn("account:default: 30 events pending", stalled[0])
        self.assertIn("stayed at 100 for 20 s", stalled[0])
        self.assertEqual(supervisor.now, 20.0)

    def test_drained_or_closed_handle_passes(self):
        stalled, _latest = FakeSupervisor([handle(100, 100)]).stalls(handle(100, 90))
        self.assertEqual(stalled, [])
        stalled, _latest = FakeSupervisor([None]).stalls(handle(100, 90))
        self.assertEqual(stalled, [])

    def test_no_pending_events_reads_nothing_more(self):
        supervisor = FakeSupervisor([handle(200, 100)])
        stalled, _latest = supervisor.stalls(handle(100, 100))
        self.assertEqual(stalled, [])
        self.assertEqual(supervisor.reads, 0)
        self.assertEqual(supervisor.now, 0.0)


if __name__ == "__main__":
    unittest.main()
