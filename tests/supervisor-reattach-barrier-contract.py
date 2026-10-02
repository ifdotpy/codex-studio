#!/usr/bin/env python3
"""A supervisor reattach barrier must finish before native callbacks start."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'persistence_fixture', Path(__file__).with_name('supervisor-persistence-retry-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class SupervisorReattachBarrier(unittest.TestCase):
    def dispatch_barrier(self, *, busy=0, fail=False):
        order, resumed = [], []
        state = {'cursor': 0, 'reattached': False}

        def native(message):
            order.append('native')
            self.assertTrue(state['reattached'], 'The barrier must precede every native callback')

        def commit(message, sequence):
            state['cursor'] = sequence

        dispatch = fixture.DispatchFixture(native, lambda _: None, lambda _: False,
                                           commit, lambda: state['cursor'])
        self.addCleanup(dispatch.close)
        server = dispatch.server
        server.supervisor_resumed = True

        def reattached(value):
            resumed.append(value)
            order.append('barrier')
            if len(resumed) <= busy:
                raise fixture.busy_error()
            if fail:
                raise fixture.io_error()
            state['reattached'] = True

        def barrier(_message):
            server.persistence_retry(lambda: reattached(server.supervisor_resumed))

        server.enqueue(barrier, {'_studioReattachBarrier': True})
        dispatch.start([{'method': 'fixture/native', '_studioSupervisorSequence': 1}])
        dispatch.wait(lambda: server.proc.cursor == 1 or server.transport_error)
        return dispatch, order, resumed

    def test_reattach_barrier_precedes_native_delivery_and_ack(self):
        dispatch, order, resumed = self.dispatch_barrier()
        self.assertEqual(order, ['barrier', 'native'])
        self.assertEqual(resumed, [True])
        self.assertEqual(dispatch.server.proc.acks, [1])
        self.assertIsNone(dispatch.server.transport_error)
        self.assertFalse(dispatch.server.proc.terminated)

    def test_busy_barrier_retries_before_native_delivery(self):
        dispatch, order, resumed = self.dispatch_barrier(busy=2)
        self.assertEqual(order, ['barrier', 'barrier', 'barrier', 'native'])
        self.assertEqual(resumed, [True, True, True])
        self.assertEqual(dispatch.server.proc.acks, [1])
        self.assertIsNone(dispatch.server.transport_error)
        self.assertFalse(dispatch.server.proc.terminated)

    def test_non_busy_barrier_failure_blocks_following_native_callbacks(self):
        dispatch, order, resumed = self.dispatch_barrier(fail=True)
        self.assertEqual(order, ['barrier'])
        self.assertEqual(resumed, [True])
        self.assertIn('disk I/O error', dispatch.server.transport_error)
        self.assertEqual(dispatch.server.proc.acks, [])
        self.assertTrue(dispatch.server.proc.terminated)
        self.assertEqual(dispatch.server.callbacks.qsize(), 1)
        queued_callback, queued_message = dispatch.server.callbacks.get_nowait()
        self.assertIs(queued_callback, dispatch.server.notification)
        self.assertEqual(queued_message['_studioSupervisorSequence'], 1)
        dispatch.server.callbacks.task_done()


if __name__ == '__main__':
    unittest.main()
