#!/usr/bin/env python3
"""Delayed native unload callbacks cannot invalidate the next preparation."""
import concurrent.futures
import copy
import importlib.util
from pathlib import Path
import queue
import threading
import unittest

spec = importlib.util.spec_from_file_location('unload_fixture', Path(__file__).with_name('context-repair-wait-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_runtime import AppServer


class PreparationUnload(f.ContextWait):
    def test_received_unload_drains_before_same_thread_resume(self):
        # An eligible saved event with no exact source match requires no fork.
        self.records[2]['payload']['internal_chat_message_metadata_passthrough']['turn_id'] = 'other'
        self.records[4]['payload']['replacement_history'][0]['internal_chat_message_metadata_passthrough']['turn_id'] = 'other'
        self.write_records()
        received, release, drained = threading.Event(), threading.Event(), threading.Event()
        callbacks = queue.Queue()
        resume = concurrent.futures.Future()
        resume_requested = threading.Event()
        original_submit = self.server.submit
        original_prepare = self.runtime.preparations.get(self.a['id'])

        def worker():
            while True:
                callback, payload = callbacks.get()
                try:
                    if callback is None:
                        return
                    callback(payload)
                finally:
                    callbacks.task_done()

        thread = threading.Thread(target=worker, daemon=True)
        thread.start()
        self.server.enqueue = lambda callback, payload: callbacks.put((callback, payload))
        self.server.after_events = lambda callback: AppServer.after_events(self.server, callback)

        def notification(_):
            received.set()
            if not release.wait(3):
                return
            self.runtime.notification({'method':'thread/status/changed',
                'params':{'threadId':self.tid,'status':{'type':'notLoaded'}}},
                'default', self.runtime.connection_ids['default'])
            drained.set()

        def submit(method, params):
            if method == 'thread/resume':
                resume_requested.set()
                self.server.calls.append((method, copy.deepcopy(params)))
                return 'exact-resume', method, resume
            return original_submit(method, params)

        self.server.submit = submit
        # The old connection already received this notice before maintenance.
        self.server.enqueue(notification, None)
        try:
            self.runtime.send(self.a['id'], 'Preserve the exact input.', message_id='unload-user')
            self.runtime.dispatch()
            self.assertTrue(received.wait(3))
            self.assertFalse(resume_requested.wait(.2))
            self.assertIs(self.runtime.preparations.get(self.a['id']), original_prepare)
            accepted = self.runtime.agent(self.a['id'])['startAttempt']
            release.set()
            f.eventually(lambda: any(m == 'thread/resume' for m,p in self.server.calls))
            self.assertTrue(drained.is_set())
            operation = self.runtime.preparations[self.a['id']]
            self.assertFalse(operation.get('unloaded'))
            resume.set_result({'thread':{'id':self.tid}, 'model':self.a['model'],
                               'sandbox':{'type':'readOnly'}, 'approvalPolicy':'on-request'})
            f.eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
            current = self.runtime.agent(self.a['id'])
            self.assertEqual(current['threadId'], self.tid)
            self.assertEqual(current['startAttempt']['id'], accepted['id'])
            self.assertEqual(current['startAttempt']['events'], ['unload-user'])
            self.assertTrue(current['startAttempt']['submitted'])
            self.assertEqual(current['contextRepair']['phase'], 'unchanged')
            self.assertEqual(self.forks(), [])
            self.assertFalse(any(m == 'thread/unsubscribe' for m,p in self.server.calls))
            self.assertEqual(len([m for m,p in self.server.calls if m == 'turn/start']), 1)
            with self.runtime.db() as db:
                event = dict(db.execute('SELECT * FROM runtime_events WHERE id=?', ('unload-user',)).fetchone())
            self.assertEqual(event['status'], 'delivered')
            self.assertEqual(event['turn_id'], 'resumed-turn')
        finally:
            release.set()
            if not resume.done():
                resume.set_exception(RuntimeError('Fixture closed'))
            callbacks.put((None, None))
            thread.join(3)


if __name__ == '__main__':
    suite = unittest.TestSuite(PreparationUnload(name) for name in PreparationUnload.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
