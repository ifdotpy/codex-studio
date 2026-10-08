#!/usr/bin/env python3
"""Runtime shutdown wakes migration workers paused for disk space."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

sys_path = str(Path(__file__).resolve().parents[1] / 'scripts')
import sys
sys.path.insert(0, sys_path)

import codex_analytics_storage
import codex_work
from codex_runtime import Runtime


class FixtureServer:
    pass


class RuntimeCloseWakeContract(unittest.TestCase):
    def test_close_interrupts_space_waiting_migration_workers(self):
        class NoFreeSpace:
            free = 0

        with tempfile.TemporaryDirectory(prefix='runtime-close-wake-') as directory:
            runtime = Runtime(Path(directory), FixtureServer)
            try:
                with runtime.lock, runtime.db() as db:
                    db.execute("UPDATE runtime_search_rollout SET phase='building' WHERE id=1")
                    db.execute("DROP TABLE IF EXISTS runtime_search_next")

                with patch.object(codex_work.shutil, 'disk_usage', return_value=NoFreeSpace()), \
                        patch.object(codex_analytics_storage, 'copy_step',
                                     return_value=(False, 'waitingForSpace', 0)):
                    self.assertTrue(runtime.search_migration_start())
                    self.assertTrue(codex_analytics_storage.start(runtime))

                    deadline = time.monotonic() + 5
                    while time.monotonic() < deadline:
                        with runtime.db() as db:
                            phase = db.execute(
                                'SELECT phase FROM runtime_search_rollout WHERE id=1'
                            ).fetchone()[0]
                        analytics_status = getattr(runtime, 'analytics_migration_status', {}).get('status')
                        if phase == 'waiting_for_space' and analytics_status == 'waitingForSpace':
                            break
                        threading.Event().wait(.01)
                    self.assertEqual(phase, 'waiting_for_space')
                    self.assertEqual(analytics_status, 'waitingForSpace')

                    close_done = threading.Event()

                    def close_runtime():
                        runtime.close()
                        close_done.set()

                    closer = threading.Thread(target=close_runtime, daemon=True)
                    closer.start()
                    self.assertTrue(close_done.wait(1),
                                    'Runtime.close() did not wake waiting migration workers')
                    closer.join()
                    self.assertFalse(runtime.search_migration_thread.is_alive())
                    self.assertFalse(runtime.analytics_migration_thread.is_alive())
            finally:
                runtime.close()


if __name__ == '__main__':
    unittest.main()
