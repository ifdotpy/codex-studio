#!/usr/bin/env python3
"""Bounded stack-only diagnostics preserve callback queue contents."""
import importlib.util
import json
from pathlib import Path
import queue
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('diagnostics', Path(__file__).parents[1] / 'scripts/codex_callback_wait_diagnostics_update.py')
d = importlib.util.module_from_spec(spec)
spec.loader.exec_module(d)


class Diagnostics(unittest.TestCase):
    def test_old_callback_triggers_bounded_capture_without_callback_or_locals(self):
        with tempfile.TemporaryDirectory() as root:
            secret_local = 'never-copy-locals-379'
            callbacks = queue.Queue()
            message = {'_studioReceivedAt':time.time()-3, 'params':{'secret':'never-copy-body-581'}}
            callback = lambda message: self.fail('The diagnostic executed a callback')
            callbacks.put((callback, message))
            runtime = type('Runtime', (), {'root':root,'closed':False,'lock':threading.RLock(),'servers':{'test':type('Server', (), {'callbacks':callbacks})()}})()
            with patch.object(d,'WINDOW_SECONDS',1), patch.object(d,'MAX_CAPTURES',2), patch.object(d,'CAPTURE_INTERVAL_SECONDS',.01):
                result = d.apply(runtime)
                self.assertEqual(result['status'], 'applied')
                self.assertEqual(d.apply(runtime)['status'], 'already_applied')
                runtime._callback_wait_diagnostic.join(2)
            self.assertFalse(runtime._callback_wait_diagnostic.is_alive())
            self.assertEqual(d.apply(runtime), {'status':'already_applied', 'observing':False})
            text = Path(result['report']).read_text()
            records = [json.loads(line) for line in text.splitlines()]
            self.assertEqual(records[-1]['status'], 'completed')
            self.assertEqual(records[-1]['captures'], 2)
            self.assertLess(records[-1]['elapsedSeconds'], 1)
            self.assertNotIn(secret_local, text)
            self.assertNotIn(message['params']['secret'], text)
            self.assertEqual(callbacks.get_nowait(), (callback,message))
            for record in records[:-1]:
                self.assertGreater(record['waits'][0]['oldestWaitMs'],1000)
                self.assertTrue(record['threads'])
                self.assertIn('_thread.RLock', record['runtimeLock'])
                for thread in record['threads']:
                    for frame in thread['stack']:
                        self.assertEqual(set(frame), {'file','line','function'})

    def test_observer_failure_reports_exception_type_without_secret_message(self):
        with tempfile.TemporaryDirectory() as root:
            runtime = type('Runtime', (), {'root':root,'closed':False,'servers':{}})()
            with patch.object(d, 'queued_waits', side_effect=RuntimeError('private-error-text')):
                result = d.apply(runtime)
                runtime._callback_wait_diagnostic.join(1)
            text = Path(result['report']).read_text()
            self.assertNotIn('private-error-text', text)
            terminal = json.loads(text)
            self.assertEqual(terminal['status'], 'failed')
            self.assertEqual(terminal['errorType'], 'RuntimeError')
            self.assertEqual(terminal['captures'], 0)

    def test_empty_queue_expires_without_captures(self):
        with tempfile.TemporaryDirectory() as root:
            runtime = type('Runtime', (), {'root':root,'closed':False,'servers':{}})()
            with patch.object(d,'WINDOW_SECONDS',.15):
                result = d.apply(runtime)
                runtime._callback_wait_diagnostic.join(1)
            records = Path(result['report']).read_text().splitlines()
            self.assertEqual(len(records),1)
            self.assertEqual(json.loads(records[0])['captures'],0)


if __name__ == '__main__':
    unittest.main()
