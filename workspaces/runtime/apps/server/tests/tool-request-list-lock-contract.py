#!/usr/bin/env python3
"""Request lists resolve pending payloads outside the runtime lock and writer."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
import json
from pathlib import Path
import sqlite3
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'workspace_fixture', Path(__file__).with_name('workspace-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_payloads import externalize_result


class ToolRequestListLockContract(unittest.TestCase):
    lead = fixture.WorkspaceContract.lead
    agent_update = fixture.WorkspaceContract.agent_update

    def setUp(self):
        fixture.WorkspaceContract.setUp(self)
        self.runtime.schedule_fast_dispatch = lambda *args, **kwargs: None
        self.threads, self.releases = [], []
        self.actor = self.agent_update(self.lead(), threadId='receipt-thread', autoWake=True)
        self.other = self.lead('Other chat')

    def tearDown(self):
        for release in self.releases:
            release.set()
        for thread in self.threads:
            thread.join(3)
        fixture.WorkspaceContract.tearDown(self)

    @staticmethod
    def result(text='done', success=True):
        return {'success': success, 'contentItems': [{'type': 'inputText', 'text': text}]}

    def receipt(self, call, updated, result=None):
        message = {'id': call, 'params': {'threadId': self.actor['threadId'], 'callId': call,
                   'tool': 'orchestration_monitor', 'arguments': {'command': 'fixture-command'}}}
        receipt = self.runtime.reserve_tool_request(message)
        self.assertTrue(self.runtime.begin_tool_request(receipt['id']))
        with self.runtime.lock, self.runtime.db() as db:
            receipt = self.runtime.tool_request(receipt['id'], db)
            receipt['updated'] = updated
            self.runtime.put(db, 'tool_requests', receipt)
            if result is not None:
                saved = externalize_result(self.runtime.root, db, result)
                db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', (receipt['id'], json.dumps(saved)))
            else:
                saved = None
        return receipt, saved

    def pending_pair(self):
        first, _ = self.receipt('first', 200, self.result())
        result = self.result('x' * 100_000)
        second, saved = self.receipt('large', 100, result)
        return first, second, saved['_payloadBlobs']['contentItems']['sha256'], result

    def raw(self, receipt):
        with self.runtime.read_db() as db:
            return db.execute('SELECT record FROM runtime_tool_requests WHERE id=?', (receipt['id'],)).fetchone()[0]

    def listing(self):
        return self.runtime.request_action(self.actor['id'], {'action': 'list'})['requests']

    def worker(self, action, name='request-list'):
        done, errors, values = threading.Event(), [], []
        def run():
            try:
                values.append(action())
            except BaseException as error:
                errors.append(error)
            finally:
                done.set()
        thread = threading.Thread(target=run, name=name, daemon=True)
        self.threads.append(thread)
        thread.start()
        return done, errors, values

    def finished(self, worker, timeout=2, failure=None):
        done, errors, values = worker
        self.assertTrue(done.wait(timeout), 'The fixture operation did not complete')
        if failure:
            self.assertEqual(len(errors), 1)
            self.assertIsInstance(errors[0], failure)
        else:
            self.assertEqual(errors, [])
        return values

    @contextmanager
    def gated_read(self, digest, count=1):
        entered, release = threading.Event(), threading.Event()
        self.releases.append(release)
        original, owned = Path.read_bytes, []
        guard = threading.Lock()
        def read(path):
            if path.name == digest and threading.current_thread().name == 'request-list':
                with guard:
                    owned.append(self.runtime.lock._is_owned())
                    if len(owned) == count:
                        entered.set()
                if not release.wait(3):
                    raise RuntimeError('The fixture payload read timed out')
            return original(path)
        with patch.object(Path, 'read_bytes', read):
            try:
                yield entered, release, owned
            finally:
                release.set()

    def test_gated_pending_payload_does_not_block_another_send(self):
        first, second, digest, body = self.pending_pair()
        with self.gated_read(digest) as (entered, release, owned):
            listing = self.worker(self.listing)
            self.assertTrue(entered.wait(2))
            try:
                self.finished(self.worker(lambda: self.runtime.send(
                    self.other['id'], 'Continue', 'payload-read-send'), 'send'), .5)
                self.assertEqual(owned, [False])
            finally:
                release.set()
            rows = self.finished(listing)[0]
        self.assertEqual({row['id']: row['outcome'] for row in rows},
                         {first['id']: 'applied', second['id']: 'applied'})
        self.assertTrue(all('result' not in row and '_payloadBlobs' not in row for row in rows))
        found = self.runtime.request_action(self.actor['id'], {'action': 'get', 'request_id': second['id']})
        self.assertEqual(found['result'], body)
        self.assertFalse(self.runtime.begin_tool_request(second['id']))
        self.assertIsNone(self.runtime.server)

    def test_gated_second_payload_does_not_hold_the_main_writer(self):
        _, _, digest, _ = self.pending_pair()
        def writer():
            db = sqlite3.connect(self.runtime.db_path, timeout=.2)
            try:
                db.execute('BEGIN IMMEDIATE')
                db.rollback()
            finally:
                db.close()
        with self.gated_read(digest) as (entered, release, _):
            listing = self.worker(self.listing)
            self.assertTrue(entered.wait(2))
            try:
                self.finished(self.worker(writer, 'writer'), .5)
            finally:
                release.set()
            self.finished(listing)

    def test_terminal_body_is_not_read_and_remains_available_to_get(self):
        receipt, _ = self.receipt('terminal', 100)
        body = self.result('terminal body ' * 10_000)
        self.runtime.finish_tool_request(receipt['id'], body)
        before = self.raw(receipt)
        with patch.object(Path, 'read_bytes', side_effect=AssertionError('Unexpected terminal body read')):
            rows = self.listing()
        self.assertEqual(rows[0]['outcome'], 'applied')
        self.assertEqual(self.raw(receipt), before)
        found = self.runtime.request_action(self.actor['id'], {'action': 'get', 'request_id': receipt['id']})
        self.assertEqual(found['result'], body)
        self.assertIsNone(self.runtime.server)

    def test_dynamic_list_and_exact_retry_use_the_supported_unlocked_path(self):
        _, receipt, _, _ = self.pending_pair()
        server = self.runtime.connect()
        before = list(server.calls)
        message = {'id': 'list-call', 'method': 'item/tool/call', 'params': {
            'threadId': self.actor['threadId'], 'callId': 'list-call',
            'tool': 'orchestration_request', 'arguments': {'action': 'list'}}}
        self.runtime.dynamic(message)
        fixture.eventually(lambda: any(row['id'] == 'list-call' for row in server.responses))
        response = next(row['result'] for row in server.responses if row['id'] == 'list-call')
        self.assertTrue(response['success'], response)
        rows = json.loads(response['contentItems'][0]['text'])['requests']
        self.assertEqual(next(row for row in rows if row['id'] == receipt['id'])['outcome'], 'applied')
        self.runtime.dynamic(message)
        fixture.eventually(lambda: sum(row['id'] == 'list-call' for row in server.responses) == 2)
        self.assertEqual(server.responses[-1]['result'], response)
        self.assertEqual(server.calls, before)

    def test_external_request_fields_cannot_supply_prepared_evidence(self):
        with self.assertRaisesRegex(ValueError, 'Unsupported request fields'):
            self.runtime.request_action(self.actor['id'], {
                'action': 'list', 'prepared_record': ('forged raw', {}, None)})

    def test_actor_stop_account_and_connection_changes_block_stale_reconciliation(self):
        for change in ('stop', 'account', 'permissions', 'connection', 'offline'):
            with self.subTest(change=change):
                self.actor = self.agent_update(self.lead(change), threadId='thread-' + change, autoWake=True)
                receipt, saved = self.receipt(change, 100, self.result(change * 30_000))
                before = self.raw(receipt)
                with self.gated_read(saved['_payloadBlobs']['contentItems']['sha256']) as (entered, release, _):
                    listing = self.worker(self.listing)
                    self.assertTrue(entered.wait(2))
                    if change == 'stop':
                        self.runtime.stop(self.actor['id'])
                    elif change == 'account':
                        self.agent_update(self.actor, accountKey='different-account')
                    elif change == 'permissions':
                        self.agent_update(self.actor, approvalPolicy='on-request')
                    elif change == 'connection':
                        with self.runtime.lock:
                            self.runtime.connection_ids['default'] = 'different-connection'
                    else:
                        with self.runtime.lock:
                            self.runtime.offline_accounts.add('default')
                    release.set()
                    self.finished(listing, failure=ValueError)
                self.assertEqual(self.raw(receipt), before)
                self.assertIsNone(self.runtime.server)

    def test_a_changed_receipt_cannot_receive_the_sampled_result(self):
        _, receipt, digest, _ = self.pending_pair()
        with self.gated_read(digest) as (entered, release, _):
            listing = self.worker(self.listing)
            self.assertTrue(entered.wait(2))
            replacement = self.result('A different authoritative result', success=False)
            self.runtime.finish_tool_request(receipt['id'], replacement, outcome='not_applied')
            release.set()
            rows = self.finished(listing)[0]
        found = next(row for row in rows if row['id'] == receipt['id'])
        self.assertEqual(found['outcome'], 'not_applied')
        self.assertEqual(self.runtime.tool_request(receipt['id'])['result'], replacement)

    def test_a_changed_saved_result_cannot_settle_the_old_payload(self):
        _, receipt, digest, _ = self.pending_pair()
        with self.gated_read(digest) as (entered, release, _):
            listing = self.worker(self.listing)
            self.assertTrue(entered.wait(2))
            with self.runtime.lock, self.runtime.db() as db:
                db.execute('UPDATE runtime_tool_results SET result=? WHERE id=?',
                           (json.dumps(self.result('New ambiguous response', success=False)), receipt['id']))
            release.set()
            rows = self.finished(listing)[0]
        found = next(row for row in rows if row['id'] == receipt['id'])
        self.assertEqual(found['outcome'], 'pending')
        self.assertNotIn('result', self.runtime.tool_request(receipt['id']))

    def test_concurrent_lists_reconcile_each_receipt_once(self):
        first, second, digest, _ = self.pending_pair()
        writes = []
        original = self.runtime.put
        def put(db, table, record, **kwargs):
            if table == 'tool_requests':
                writes.append(record['id'])
            return original(db, table, record, **kwargs)
        with self.gated_read(digest, count=2) as (entered, release, owned), patch.object(self.runtime, 'put', put):
            one = self.worker(self.listing)
            two = self.worker(self.listing)
            self.assertTrue(entered.wait(2))
            release.set()
            for worker in (one, two):
                rows = self.finished(worker)[0]
                self.assertTrue(all(row['outcome'] == 'applied' for row in rows))
        self.assertEqual(owned, [False, False])
        self.assertCountEqual(writes, [first['id'], second['id']])
        self.assertIsNone(self.runtime.server)

    def test_prepared_record_cannot_substitute_identity_or_saved_evidence(self):
        receipt, _ = self.receipt('guarded', 100, self.result('Saved result'))
        raw = self.raw(receipt)
        with self.runtime.read_db() as db:
            saved = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (receipt['id'],)).fetchone()[0]
        for prepared in ((raw, {**json.loads(raw), 'epoch': 99}, saved),
                         (raw, {**json.loads(raw), 'id': 'another-receipt'}, saved),
                         (raw, json.loads(raw), json.dumps(self.result('Another saved result')))):
            with self.subTest(prepared=prepared[1]['id']):
                with self.runtime.lock, self.runtime.db() as db:
                    with self.assertRaisesRegex(ValueError, 'receipt changed'):
                        self.runtime.finish_tool_request(receipt['id'], self.result(), db=db,
                                                         prepared_record=prepared)
                self.assertEqual(self.raw(receipt), raw)

    def test_unknown_payload_and_read_failure_preserve_exact_receipts(self):
        receipt, _ = self.receipt('unknown', 100)
        body = self.result('Ambiguous ' * 10_000, success=False)
        self.runtime.finish_tool_request(receipt['id'], body)
        before = self.raw(receipt)
        rows = self.listing()
        self.assertEqual(rows[0]['outcome'], 'unknown')
        self.assertEqual(self.raw(receipt), before)
        self.assertEqual(self.runtime.tool_request(receipt['id'])['result'], body)
        with patch.object(Path, 'read_bytes', side_effect=OSError('Fixture unavailable payload')):
            with self.assertRaisesRegex(OSError, 'unavailable payload'):
                self.listing()
        self.assertEqual(self.raw(receipt), before)
        self.assertFalse(self.runtime.begin_tool_request(receipt['id']))


if __name__ == '__main__':
    unittest.main()
