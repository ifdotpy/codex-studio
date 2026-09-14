#!/usr/bin/env python3
"""Recover a paused worker receipt through its managed default account home."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
from types import ModuleType, SimpleNamespace
import sys
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('terminal_fixture',
    Path(__file__).with_name('terminal-receipt-recovery-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_worker_terminal_receipt_recovery_update as worker


class PausedWorkerRecovery(f.TerminalReceiptRecoveryContract):
    def setUp(self):
        super().setUp()
        self.plan_patch.stop()
        self.original_route_plan = copy.deepcopy(f.update.PLAN)
        self.plan.update(id='paused-worker-test', accountKey='default', currentEpoch=1,
                         requirePaused=True, accountHomePolicy='managed')
        previous_id = self.saved['id']
        self.saved.update(accountKey='default', id=self.plan['threadId'] + ':' + self.plan['callId'])
        self.actor.update(accountKey='default', epoch=1, status='paused', autoWake=False, inFlight=False)
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_tool_requests WHERE id=?', (previous_id,))
            self.runtime.put(db, 'tool_requests', self.saved)
            db.execute('UPDATE runtime_agents SET record=? WHERE id=?', (json.dumps(self.actor), self.actor['id']))
        self.managed_home = self.root / 'native-default-home'
        target = self.managed_home / 'sessions' / self.path.name
        target.parent.mkdir(parents=True)
        self.path.replace(target)
        self.path = target
        self.evidence['sourcePath'] = str(target)
        self.write_evidence()
        self.runtime.accounts = SimpleNamespace(home=Mock(return_value=self.managed_home))
        self.worker_plan_patch = patch.object(worker, 'PLAN', self.plan)
        self.worker_plan_patch.start()
        self.addCleanup(self.worker_plan_patch.stop)
        self.original_loader = worker._load_helper
        self.helper = self.original_loader()
        self.loader_patch = patch.object(worker, '_load_helper', return_value=self.helper)
        self.loader_patch.start()
        self.addCleanup(self.loader_patch.stop)

    def apply(self):
        return worker.apply(self.runtime)

    def test_original_request_epoch_and_current_pause_survive_without_global_plan_change(self):
        before_actor = copy.deepcopy(self.actor)
        self.assertEqual(self.apply()['status'], 'applied')
        self.runtime.accounts.home.assert_called_once_with('default')
        with self.runtime.db() as db:
            self.assertEqual(json.loads(db.execute('SELECT record FROM runtime_agents').fetchone()[0]), before_actor)
            receipt = self.runtime.tool_request(self.saved['id'], db)
            self.assertEqual(receipt['epoch'], 0)
            self.assertEqual(receipt['outcome'], 'unknown')
        self.assertEqual(f.update.PLAN, self.original_route_plan)

    def test_changed_pause_or_current_epoch_refuses_without_writes(self):
        for field, value in (('status', 'completed'), ('autoWake', True), ('inFlight', True), ('epoch', 0), ('epoch', 2)):
            with self.subTest(field=field, value=value):
                with self.runtime.db() as db:
                    db.execute('UPDATE runtime_agents SET record=?', (json.dumps({**self.actor, field: value}),))
                before = self.state()
                with self.assertRaises(RuntimeError):
                    self.apply()
                self.assertEqual(self.state(), before)

    def test_changed_managed_home_refuses_without_writes(self):
        wrong_home = self.root / 'wrong-home'
        wrong_home.mkdir()
        self.runtime.accounts.home.return_value = wrong_home
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'escapes its account'):
            self.apply()
        self.assertEqual(self.state(), before)

    def test_native_source_changes_during_read_reject(self):
        original = self.helper._record
        def changing(source, proof):
            record = original(source, proof)
            with self.path.open('ab') as output:
                output.write(b'\n')
            return record
        before = self.state()
        with patch.object(self.helper, '_record', side_effect=changing):
            with self.assertRaisesRegex(RuntimeError, 'source changed during proof read'):
                self.apply()
        self.assertEqual(before, self.state())

    def test_native_ordinals_do_not_need_to_equal_physical_line_numbers(self):
        header = self.path.read_bytes().splitlines(keepends=True)[0]
        content = header
        for key in ('receipt', 'terminalTurn'):
            proof = self.evidence[key]
            proof['record']['ordinal'] += 1000
            raw = (json.dumps(proof['record']) + '\n').encode()
            proof.update(byteOffset=len(content), rawRecord=raw.decode(), recordSha256=hashlib.sha256(raw).hexdigest())
            content += raw
        self.path.write_bytes(content)
        self.evidence['sourceObservation']['size'] = len(content)
        self.write_evidence()
        self.assertEqual(self.apply()['status'], 'applied')

    def test_cached_old_helper_is_not_reused_or_modified(self):
        previous = ModuleType('codex_terminal_receipt_recovery_update')
        previous.sentinel = object()
        before = vars(previous).copy()
        with patch.dict(sys.modules, {'codex_terminal_receipt_recovery_update': previous}), \
                patch.object(worker, '_load_helper', side_effect=self.original_loader):
            self.assertEqual(self.apply()['status'], 'applied')
            self.assertIs(sys.modules['codex_terminal_receipt_recovery_update'], previous)
        self.assertEqual(vars(previous), before)

    def test_terminal_must_follow_the_receipt_in_bytes_and_native_ordinals(self):
        original = copy.deepcopy(self.evidence)
        header = self.path.read_bytes().splitlines(keepends=True)[0]
        for bad_order in ('ordinal', 'bytes'):
            with self.subTest(order=bad_order):
                self.evidence = copy.deepcopy(original)
                keys = ('receipt', 'terminalTurn') if bad_order == 'ordinal' else ('terminalTurn', 'receipt')
                if bad_order == 'ordinal':
                    self.evidence['terminalTurn']['record']['ordinal'] = self.evidence['receipt']['record']['ordinal']
                content = header
                for key in keys:
                    proof = self.evidence[key]
                    raw = (json.dumps(proof['record']) + '\n').encode()
                    proof.update(byteOffset=len(content), rawRecord=raw.decode(), recordSha256=hashlib.sha256(raw).hexdigest())
                    content += raw
                self.path.write_bytes(content)
                self.evidence['sourceObservation']['size'] = len(content)
                self.write_evidence()
                before = self.state()
                with self.assertRaisesRegex(RuntimeError, 'matching later completion'):
                    self.apply()
                self.assertEqual(self.state(), before)

    def test_unreviewed_helper_bytes_refuse_before_receipt_write(self):
        before = self.state()
        with patch.object(worker, 'HELPER_SHA256', 'wrong'), \
                patch.object(worker, '_load_helper', side_effect=self.original_loader):
            with self.assertRaisesRegex(RuntimeError, 'unreviewed helper source'):
                self.apply()
        self.assertEqual(self.state(), before)


if __name__ == '__main__':
    unittest.main()
