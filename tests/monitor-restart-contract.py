#!/usr/bin/env python3
"""Monitor results survive a database rollback and an independent process restart."""
import importlib.util
import base64
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_monitor_recovery import (persist_monitor_result, acknowledge_monitor_result,
                                    recover_monitor_results, _path)
spec = importlib.util.spec_from_file_location('monitor_fixture', ROOT / 'tests/monitor-lifecycle-contract.py')
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class MonitorRestart(unittest.TestCase):
    setUp = f.MonitorLifecycleContract.setUp
    tearDown = f.MonitorLifecycleContract.tearDown
    record = f.MonitorLifecycleContract.record
    exits = f.MonitorLifecycleContract.exits

    def saved(self, *, epoch=None, paused=False, status='lost', code=19, rule=None):
        monitor = self.runtime.monitor(self.agent['id'], {'command': 'never-run'},
            key=('rule:' + rule['id'] + ':1') if rule else None)
        operation = {'agent': self.agent['id'], 'epoch': self.agent['epoch'],
                     'accountKey': 'default', 'connectionId': 'saved-connection'}
        if epoch is not None:
            operation['epoch'] = epoch
        with self.runtime.lock, self.runtime.db() as db:
            monitor.update(status=status, operation=operation, epoch=operation['epoch'])
            if rule:
                monitor['ruleId'] = rule['id']
            self.runtime.put(db, 'monitors', monitor)
            if paused:
                actor = self.runtime.agent(self.agent['id'], db)
                actor.update(autoWake=False, status='paused')
                self.runtime.put(db, 'agents', actor)
        receipt = {'code': code, 'error': None, 'operation': operation,
                   'finished': time.time() - 60}
        persist_monitor_result(self.runtime.root, monitor['id'], receipt)
        return monitor, receipt

    def recover(self):
        with self.runtime.lock, self.runtime.db() as db:
            return recover_monitor_results(self.runtime, db)

    def test_monitor_log_keeps_output_beyond_previous_disk_cap(self):
        m, _ = self.saved(status='running')
        payload = b'x' * (20 * 1024 * 1024) + b'COMPLETE-END'
        connection = self.runtime.connection_ids['default']
        self.runtime.output({'processId':m['id'], 'deltaBase64':base64.b64encode(payload).decode()},
                            'default', connection)
        self.assertEqual(Path(m['log']).read_bytes(), payload)
        self.assertLessEqual(len(self.record(m['id'])['tail']),12000)

    def test_independent_writer_exit_and_database_rollback_recover_once(self):
        m, receipt = self.saved()
        acknowledge_monitor_result(self.runtime.root, m['id'])
        subprocess.run([sys.executable, '-B', '-c',
            'import sys,json; sys.path.insert(0,sys.argv[1]); '
            'from codex_monitor_recovery import persist_monitor_result; '
            'persist_monitor_result(sys.argv[2],sys.argv[3],json.loads(sys.argv[4]))',
            str(ROOT / 'scripts'), str(self.runtime.root), m['id'], json.dumps(receipt)], check=True)
        with self.runtime.lock:
            with self.assertRaisesRegex(sqlite3.OperationalError, 'injected rollback'):
                with self.runtime.db() as db:
                    recover_monitor_results(self.runtime, db)
                    raise sqlite3.OperationalError('injected rollback')
            self.assertEqual(self.record(m['id'])['status'], 'lost')
            self.assertTrue(_path(self.runtime.root, m['id']).exists())
            result = self.recover()
            self.assertEqual(result['restored'], [m['id']])
            self.assertEqual(self.record(m['id'])['exitCode'], 19)
            self.assertEqual(self.record(m['id'])['finished'], receipt['finished'])
            self.assertEqual(len(self.exits(m['id'])), 1)
            self.assertEqual(self.recover()['restored'], [])
            self.assertEqual(len(self.exits(m['id'])), 1)
            acknowledge_monitor_result(self.runtime.root, m['id'])
            self.assertFalse(_path(self.runtime.root, m['id']).exists())
        self.assertEqual(self.server.commands, {})

    def test_first_receipt_is_immutable(self):
        m, receipt = self.saved()
        first = _path(self.runtime.root, m['id']).read_bytes()
        second = persist_monitor_result(self.runtime.root, m['id'], {**receipt, 'code': 0})
        self.assertEqual(second['code'], 19)
        self.assertEqual(_path(self.runtime.root, m['id']).read_bytes(), first)
        self.assertEqual(_path(self.runtime.root, m['id']).stat().st_mode & 0o777, 0o600)

    def test_stopped_and_replaced_owners_do_not_wake(self):
        for kwargs in ({'epoch': -1}, {'paused': True}):
            with self.subTest(kwargs=kwargs), self.runtime.lock:
                m, _ = self.saved(**kwargs)
                self.recover()
                self.assertEqual(self.record(m['id'])['status'], 'failed')
                self.assertEqual(self.exits(m['id'])[0]['status'], 'cancelled')

    def test_wrong_operation_and_corrupt_bytes_are_preserved_and_reported(self):
        m, _ = self.saved()
        with self.runtime.lock, self.runtime.db() as db:
            value = self.record(m['id'])
            value['operation']['connectionId'] = 'different-submission'
            self.runtime.put(db, 'monitors', value)
        corrupt = _path(self.runtime.root, 'corrupt')
        corrupt.write_text('{broken')
        result = self.recover()
        self.assertEqual(result['restored'], [])
        self.assertEqual(len(result['warnings']), 2)
        self.assertEqual(corrupt.read_text(), '{broken')
        self.assertTrue(_path(self.runtime.root, m['id']).exists())
        self.assertEqual(self.record(m['id'])['status'], 'lost')

    def test_unknown_command_has_no_inferred_exit(self):
        m, receipt = self.saved()
        acknowledge_monitor_result(self.runtime.root, m['id'])
        with self.assertRaisesRegex(ValueError, 'no definitive outcome'):
            persist_monitor_result(self.runtime.root, m['id'], {**receipt, 'code': None})
        self.assertEqual(self.recover()['restored'], [])
        self.assertIsNone(self.record(m['id'])['exitCode'])
        self.assertEqual(self.server.commands, {})

    def test_storage_failure_rolls_back_result_and_keeps_journal(self):
        m, _ = self.saved()
        with patch.object(self.runtime, '_monitor_exit_event', side_effect=OSError('disk unavailable')):
            with self.assertRaisesRegex(OSError, 'disk unavailable'):
                self.recover()
        self.assertEqual(self.record(m['id'])['status'], 'lost')
        self.assertTrue(_path(self.runtime.root, m['id']).exists())
        self.assertEqual(self.recover()['restored'], [m['id']])

    def test_result_received_after_close_is_recovered_by_real_runtime_startup(self):
        monitor, receipt = self.saved(status='running', code=7)
        acknowledge_monitor_result(self.runtime.root, monitor['id'])
        self.runtime.close()
        self.runtime.finish_monitor(monitor['id'], 7, None, operation=receipt['operation'])
        self.assertTrue(_path(self.runtime.root, monitor['id']).exists())
        self.runtime = f.Runtime(Path(self.temp.name), f.MonitorServer)
        self.assertEqual(self.record(monitor['id'])['status'], 'failed')
        self.assertEqual(self.record(monitor['id'])['exitCode'], 7)
        self.assertEqual(len(self.exits(monitor['id'])), 1)
        self.assertFalse(_path(self.runtime.root, monitor['id']).exists())
        self.assertFalse(any(server.commands for server in self.runtime.servers.values()))

    def test_rule_recovery_preserves_exact_check_and_explicit_pause(self):
        for state in ('active', 'paused', 'replaced', 'held'):
            with self.subTest(state=state), self.runtime.lock:
                actor = self.runtime.agent(self.agent['id'])
                with self.runtime.db() as db:
                    actor.update(autoWake=True, status='completed')
                    actor.pop('restartRecovery', None)
                    self.runtime.put(db, 'agents', actor)
                rule = self.runtime.rules_action({'action': 'save', 'agent': actor['id'],
                    'name': 'Recover ' + state, 'kind': 'interval', 'intervalSeconds': 3600,
                    'command': 'never-run'})
                monitor, _ = self.saved(rule=rule, code=0)
                with self.runtime.db() as db:
                    rule.update(inFlight=True, checks=2 if state == 'replaced' else 1,
                                status='paused' if state == 'paused' else 'active')
                    self.runtime.put(db, 'rules', rule)
                    self.runtime.setup_rules(db)
                    if state == 'held':
                        actor['restartRecovery'] = {name: actor.get(name) for name in
                            ('epoch', 'accountKey', 'threadId')}
                        actor['restartRecovery'].update(stage='pending', autoWake=True)
                        actor.update(autoWake=False, status='interrupted')
                        self.runtime.put(db, 'agents', actor)
                    recover_monitor_results(self.runtime, db)
                    current = json.loads(db.execute('SELECT record FROM runtime_rules WHERE id=?',
                                                   (rule['id'],)).fetchone()[0])
                    event = db.execute('SELECT status FROM runtime_events WHERE id=?',
                                       ('rule-wake:' + rule['id'] + ':1',)).fetchone()
                if state in ('active', 'held'):
                    self.assertEqual(current['status'], 'active')
                    self.assertEqual(current['lastExitCode'], 0)
                    self.assertEqual(event[0], 'pending')
                    self.assertNotIn('restartCheck', current)
                else:
                    self.assertEqual(current['status'], 'paused')
                    self.assertIsNone(event)
                    if state == 'replaced':
                        self.assertNotIn('lastExitCode', current)
                self.assertEqual(self.record(monitor['id'])['exitCode'], 0)


if __name__ == '__main__':
    unittest.main(verbosity=2)
