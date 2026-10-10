#!/usr/bin/env python3
"""A full scheduler and a large roster must drain a 15-turn notification storm."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    'notification_load', Path(__file__).with_name('notification-load-contract.py'))
notification_load = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notification_load)


class NotificationScheduler(unittest.TestCase):
    def test_fifteen_turn_storm_with_real_scheduler(self):
        result = notification_load.measure(1500, 15, True,
                                           live_scheduler=True, idle_roster=1800)
        self.assertEqual(result['journalAcked'], 1500)
        self.assertEqual(result['stormAgents'], 15)
        self.assertEqual(result['idleRoster'], 1800)
        self.assertGreater(result['processedCallbacksPerSecond'], 20)


if __name__ == '__main__':
    unittest.main()
