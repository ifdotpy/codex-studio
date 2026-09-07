#!/usr/bin/env python3
"""Panel feed leases, state ownership, render rejection, and request identity."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('panel_fixture', Path(__file__).with_name('panel-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class FeedContract(unittest.TestCase):
    tearDown = f.PanelContract.tearDown
    lead = f.PanelContract.lead
    agent_update = f.PanelContract.agent_update

    def setUp(self):
        f.PanelContract.setUp(self)
        self.actor = self.lead()
        self.runtime.panel_action(self.actor['id'], {'action': 'set', 'spec': {
            'root': 'root', 'elements': {'root': {'type': 'Text', 'props': {'text': {'$state': '/live/label'}}}},
            'state': {'live': {'label': 'Initial'}, 'selected': 'tab-1'}},
            'callbacks': [{'id': 'inspect', 'label': 'Inspect'}]})
        self.launch = patch.object(self.runtime, 'launch_monitor')
        self.launch_mock = self.launch.start()
        self.addCleanup(self.launch.stop)

    def start_feed(self, key='feed-start'):
        result = self.runtime.panel_feed_action(self.actor['id'], {'action': 'start', 'command': 'fixture-feed'}, key)
        with self.runtime.lock, self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_monitors WHERE id=?', (result['monitorId'],)).fetchone()
            monitor = json.loads(row[0])
            monitor['status'] = 'running'
            self.runtime.put(db, 'monitors', monitor)
        self.runtime.panel_feed_status(self.actor['id'], result['monitorId'], result['version'], '/live', 'running')
        return result

    def frame(self, feed, state, sequence=1):
        return self.runtime.panel_feed_update(self.actor['id'], feed['monitorId'], feed['version'], '/live', state, sequence)

    def test_feed_data_preserves_template_callbacks_local_state_and_history(self):
        feed = self.start_feed()
        with self.runtime.db() as db:
            before = db.execute('SELECT COUNT(*) FROM runtime_events').fetchone()[0]
        revision = self.runtime.ui_revisions.get(self.actor['id'])
        self.assertTrue(self.frame(feed, {'label': 'Updated'}))
        self.assertEqual(self.runtime.ui_revisions.get(self.actor['id']), revision)
        panel = self.runtime.panel(self.actor['id'])
        self.assertEqual(panel['version'], feed['version'])
        self.assertEqual(panel['spec']['state'], {'live': {'label': 'Updated'}, 'selected': 'tab-1'})
        self.assertEqual(panel['callbacks'][0]['id'], 'inspect')
        self.assertEqual(self.runtime.agent(self.actor['id'])['panelDataVersion'], panel['dataVersion'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events').fetchone()[0], before)
        captures = self.mock_capture.call_count
        self.assertTrue(self.frame(feed, {'label': 'Updated'}, 2))
        self.assertEqual(self.mock_capture.call_count, captures)
        self.assertFalse(self.frame(feed, {'label': 'Old'}, 1))

    def test_rejected_layout_retains_last_snapshot(self):
        feed = self.start_feed()
        before = self.runtime.panel(self.actor['id'])
        self.mock_capture.side_effect = ValueError('Panel content overflows 150px')
        with self.assertRaisesRegex(ValueError, 'overflows'):
            self.frame(feed, {'label': 'Too big'})
        self.assertEqual(self.runtime.panel(self.actor['id']), before)

    def test_cancel_during_render_rejects_commit(self):
        feed = self.start_feed()
        def cancel_during_capture(*args, **kwargs):
            with self.runtime.lock, self.runtime.db() as db:
                record = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?', (feed['monitorId'],)).fetchone()[0])
                record['cancelRequested'] = True
                self.runtime.put(db, 'monitors', record)
            return {}
        self.mock_capture.side_effect = cancel_during_capture
        self.assertFalse(self.frame(feed, {'label': 'Late'}))
        self.assertEqual(self.runtime.panel(self.actor['id'])['spec']['state']['live']['label'], 'Initial')

    def test_idempotent_start_and_replaced_template_fence(self):
        feed = self.start_feed()
        again = self.runtime.panel_feed_action(self.actor['id'], {'action': 'start', 'command': 'fixture-feed'}, 'feed-start')
        self.assertEqual(again, feed)
        self.assertEqual(self.launch_mock.call_count, 1)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.runtime.panel_feed_action(self.actor['id'], {'action': 'start', 'command': 'other'}, 'feed-start')
        self.runtime.panel_action(self.actor['id'], {'action': 'set', 'html': '<b>New panel</b>'})
        self.assertNotIn('feed', self.mock_capture.call_args.args[0])
        self.assertFalse(self.frame(feed, {'label': 'Late'}))

    def test_terminal_status_does_not_regress_and_stop_settles(self):
        feed = self.start_feed()
        with self.runtime.lock, self.runtime.db() as db:
            record = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?', (feed['monitorId'],)).fetchone()[0])
            record['status'] = 'completed'
            self.runtime.put(db, 'monitors', record)
        self.runtime.panel_feed_status(self.actor['id'], feed['monitorId'], feed['version'], '/live', 'completed')
        self.assertFalse(self.runtime.panel_feed_status(self.actor['id'], feed['monitorId'], feed['version'], '/live', 'starting'))
        self.runtime.panel_feed_action(self.actor['id'], {'action': 'stop'})
        self.assertEqual(self.runtime.panel(self.actor['id'])['feed']['status'], 'completed')

    def test_reserved_feed_stop_without_monitor(self):
        feed = self.start_feed()
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('DELETE FROM runtime_monitors WHERE id=?', (feed['monitorId'],))
        self.runtime.panel_feed_action(self.actor['id'], {'action': 'stop'})
        self.assertEqual(self.runtime.panel(self.actor['id'])['feed']['status'], 'cancelled')

    def test_bad_shape_epoch_and_ownership_fail_before_launch(self):
        for data in ({'action': 'start', 'command': 'x', 'statePath': '/live/nested'},
                     {'action': 'start', 'command': 'x', 'statePath': '/constructor'},
                     {'action': 'start', 'command': 'x', 'timeout_ms': True},
                     {'action': 'start', 'command': 'x', 'agent': 'other'},
                     {'action': 'get', 'statePath': '/live'}):
            with self.assertRaises(ValueError):
                self.runtime.panel_feed_action(self.actor['id'], data)
        self.assertEqual(self.launch_mock.call_count, 0)
        self.agent_update(self.actor, epoch=self.actor['epoch'] + 1)
        with self.assertRaisesRegex(ValueError, 'stopped'):
            self.runtime.panel_feed_action(self.actor['id'], {'action': 'start', 'command': 'x'}, epoch=self.actor['epoch'])

    def test_restart_retains_data_marks_lost_and_never_replays(self):
        feed = self.start_feed()
        self.frame(feed, {'label': 'Last snapshot'})
        self.runtime.close()
        self.runtime = f.f.ControlledRuntime(self.state, f.f.WorkspaceServer)
        panel = self.runtime.panel(self.actor['id'])
        self.assertEqual(panel['feed']['status'], 'lost')
        self.assertEqual(panel['spec']['state']['live']['label'], 'Last snapshot')
        self.assertEqual(self.runtime.servers, {})


if __name__ == '__main__':
    unittest.main()
