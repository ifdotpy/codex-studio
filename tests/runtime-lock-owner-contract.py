#!/usr/bin/env python3
"""The metrics wrapper preserves the real RLock owner for runtime diagnostics."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import os
from pathlib import Path
import re
import sys
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_lock_metrics import MeasuredRLock, runtime_lock
from codex_runtime import Runtime


class RuntimeLockOwnerContract(unittest.TestCase):
    def test_repr_preserves_owner_identity_and_reentrant_count(self):
        inner = threading.RLock()
        lock = MeasuredRLock(inner)
        self.assertEqual(repr(lock), repr(inner))
        with lock:
            with lock:
                before = lock.runtime_lock_metrics()
                self.assertEqual(repr(lock), repr(inner))
                self.assertIn('owner=' + str(threading.get_ident()), repr(lock))
                self.assertIn('count=2', repr(lock))
                self.assertEqual(lock.runtime_lock_metrics(), before)
        self.assertEqual(repr(lock), repr(inner))
        self.assertIn('owner=0', repr(lock))
        self.assertIn('count=0', repr(lock))
        metrics = lock.runtime_lock_metrics()
        self.assertEqual(len(metrics), 1)
        self.assertEqual(metrics[0]['count'], 1)

    def test_runtime_sampler_finds_the_actual_owner_thread_with_metrics_enabled(self):
        with patch.dict(os.environ, {'CODEX_RUNTIME_LOCK_METRICS': '1'}):
            lock = runtime_lock()
        self.assertIsInstance(lock, MeasuredRLock)
        runtime = Runtime.__new__(Runtime)
        runtime.lock = lock
        ready, release = threading.Event(), threading.Event()
        namespace = {'runtime': runtime, 'ready': ready, 'release': release}
        exec(compile('''def hold_runtime_lock():
    with runtime.lock:
        with runtime.lock:
            ready.set()
            release.wait(2)
''', 'codex_lock_owner_fixture.py', 'exec'), namespace)
        thread = threading.Thread(target=namespace['hold_runtime_lock'])
        thread.start()
        try:
            self.assertTrue(ready.wait(1), 'The fixture thread must hold the real lock')
            self.assertFalse(lock.acquire(blocking=False))
            observations = []
            runtime.sample_dispatch_lock_holder(observations, 37)
            self.assertEqual(observations,
                             ['37ms:codex_lock_owner_fixture.py|hold_runtime_lock'])
            self.assertEqual(repr(lock), repr(lock._lock))
            owner = re.search(r'owner=(\d+)', repr(lock))
            self.assertIsNotNone(owner)
            self.assertEqual(int(owner.group(1)), thread.ident)
        finally:
            release.set()
            thread.join(1)
        self.assertFalse(thread.is_alive())
        self.assertEqual(lock.runtime_lock_metrics()[0]['count'], 1)


if __name__ == '__main__':
    unittest.main()
