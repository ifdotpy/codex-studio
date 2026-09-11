#!/usr/bin/env python3
"""Receipt repair preserves ownership, failed mutations, and archive evidence."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_tool_requests import RequestMixin
from codex_efficiency import EfficiencyMixin
from codex_agent_management import manage_agent
from codex_request_recovery import recover_legacy_requests


def fixture(name):
    spec = importlib.util.spec_from_file_location(name.replace('-', '_'), ROOT / 'tests' / (name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


requests = fixture('tool-request-contract')
management = fixture('agent-management-contract')
legacy = fixture('request-recovery-legacy-contract')


class Receipts(unittest.TestCase):
    setUp = requests.RequestContract.setUp
    message = requests.RequestContract.message
    reserve = requests.RequestContract.reserve
    result = requests.RequestContract.result
    save_operation_receipt = requests.RequestContract.save_operation_receipt

    def failed_request(self, call, tool, args=None, error='limit must be 1 to 50', legacy=False):
        record = self.reserve(call=call, tool=tool, args=args or {})
        self.runtime.begin_tool_request(record['id'])
        result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': error}]}
        if legacy:
            record.update(stage='failed', outcome='unknown', result=result, finished=123.0)
            record.pop('readOnly', None)
            with self.runtime.db() as db:
                self.runtime.put(db, 'tool_requests', record)
        else:
            record = self.runtime.finish_tool_request(record['id'], result)
        return record

    def test_new_read_only_and_workspace_bridge_failures_are_definitive(self):
        for index, (tool, args) in enumerate([
            ('orchestration_task', {'action': 'get'}),
            ('orchestration_read', {}),
            ('orchestration_send', {'agent_id': 'workspace', 'text': json.dumps({
                'tool': 'orchestration_task', 'arguments': {'action': 'list'}})}),
        ]):
            record = self.failed_request(str(index), tool, args, error='Database read failed')
            self.assertEqual(record['outcome'], 'not_applied')
            self.assertFalse(self.runtime.begin_tool_request(record['id']))

    def test_legacy_read_validation_reconciles_from_record_without_cached_result(self):
        for index, tool in enumerate(['orchestration_peers', 'orchestration_task', 'orchestration_send']):
            record = self.failed_request(str(index), tool, legacy=True)
            found = self.runtime.request_action('lead', {'action': 'get', 'request_id': record['id']})
            self.assertEqual(found['outcome'], 'not_applied')
            self.assertEqual(found['result'], record['result'])
            self.assertEqual(found['finished'], 123.0)
            self.assertEqual(self.runtime.tool_request(record['id'])['outcome'], 'not_applied')

    def test_mutation_error_is_unknown_even_when_it_mentions_validation(self):
        for index, (tool, error) in enumerate([
            ('orchestration_task', 'Disk write failed'), ('orchestration_monitor', 'limit must be 1 to 50'),
            ('orchestration_panel', 'The panel renderer timed out after 15 seconds.'),
            ('orchestration_send', '{"code": -32600, "message": "no active turn to steer"}'),
        ]):
            record = self.failed_request(str(index), tool, error=error, legacy=True)
            found = self.runtime.request_action('lead', {'action': 'get', 'request_id': record['id']})
            self.assertEqual(found['outcome'], 'unknown')
            self.assertFalse(self.runtime.begin_tool_request(record['id']))

    def test_committed_operation_prevents_false_not_applied_and_remains_visible(self):
        record = self.failed_request('committed', 'orchestration_task', legacy=True)
        self.save_operation_receipt(record['id'], {'id': 'already-written', 'status': 'accepted'})
        found = self.runtime.request_action('lead', {'action': 'get', 'request_id': record['id']})
        self.assertEqual(found['outcome'], 'unknown')
        self.assertTrue(found['operationApplied'])
        self.assertEqual(found['operationResult']['id'], 'already-written')
        self.assertEqual(found['result'], record['result'])

    def test_legacy_receipts_follow_recorded_transfer_but_do_not_cross_agents(self):
        with self.runtime.db() as db:
            actor = self.runtime.checked_actor(db, 'lead')
            actor.update(threadId='new-thread', accountKey='account-new', accountHistory=[{
                'accountKey': 'default', 'threadId': 'thread', 'transferId': 'saved-transfer'}])
            self.runtime.put(db, 'agents', actor)
            for key in ['thread:old', 'account-new:new-thread:collision', 'thread:collision']:
                db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', (key, json.dumps(self.result())))
        for key in ['old', 'thread:old']:
            self.assertEqual(self.runtime.request_action('lead', {'action': 'get', 'request_id': key})['outcome'], 'applied')
        self.assertEqual(self.runtime.request_action('lead', {'action': 'get', 'request_id': 'collision'})['stage'], 'ambiguous')
        self.assertEqual(self.runtime.request_action('worker', {'action': 'get', 'request_id': 'thread:old'})['stage'], 'not_found')


class Archives(unittest.TestCase):
    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        class Store(management.Store, RequestMixin):
            pass
        self.rt = Store(str(Path(self.tmp.name) / 'state.sqlite'))
        with self.rt.db() as db:
            db.execute('CREATE TABLE runtime_tool_results(id TEXT PRIMARY KEY, result TEXT)')
            self.rt.setup_tool_requests(db)

    def call(self, action):
        return manage_agent(self.rt, 'lead', {'action': action, 'agent_id': 'worker', 'reason': 'Verified'}, 1)

    def record(self, tool, error):
        result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': error}]}
        with self.rt.db() as db:
            self.rt.put(db, 'tool_requests', {'id': 'old-account:old-thread:request', 'agent': 'worker',
                'tool': tool, 'stage': 'failed', 'outcome': 'unknown', 'result': result})

    def test_inspect_reconciles_read_failure_then_archive_preserves_receipt_and_history(self):
        self.record('orchestration_peers', 'limit must be 1 to 50')
        self.assertTrue(self.call('inspect')['canArchive'])
        self.assertEqual(self.call('archive')['status'], 'archived')
        with self.rt.db() as db:
            self.assertEqual(self.rt.records(db, 'tool_requests')[0]['outcome'], 'not_applied')
            self.assertEqual(self.rt.records(db, 'items')[0]['text'], 'Preserved result')
        self.assertEqual(self.rt.calls, [('thread/read', 'worker')])

    def test_recover_reconciles_target_receipts_without_replay(self):
        self.record('orchestration_task', 'limit must be 1 to 50')
        result = self.call('recover')
        self.assertEqual(result['requests']['reconciled'], ['old-account:old-thread:request'])
        self.assertEqual(self.rt.calls, [('reconcile', 'worker')])

    def test_unknown_mutation_still_blocks_archive_and_recover_does_not_erase_it(self):
        self.record('orchestration_monitor', 'Native response lost')
        self.assertFalse(self.call('inspect')['canArchive'])
        self.assertEqual(self.call('recover')['requests']['unknown'], ['old-account:old-thread:request'])
        self.assertEqual(self.call('archive')['status'], 'blocked')
        with self.rt.db() as db:
            self.assertEqual(self.rt.records(db, 'tool_requests')[0]['outcome'], 'unknown')


class OutputReads(unittest.TestCase):
    def setUp(self):
        self.case = requests.RequestContract()
        self.case.setUp()
        self.addCleanup(self.case.doCleanups)
        class Store(requests.Ledger, EfficiencyMixin):
            def checked_actor(self, db, agent_id, *args):
                return super().checked_actor(db, agent_id)
        self.rt = Store(self.case.runtime.path)
        with self.rt.db() as db:
            db.execute('CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT)')
            item = {'id': 'lead:exec-old', 'title': 'commandExecution', 'truncated': True,
                'text': json.dumps({'status': 'completed', 'exitCode': 0, 'aggregatedOutput': 'Verified output\n' * 1000})}
            db.execute('INSERT INTO runtime_items VALUES (?,?,?)', (item['id'], 'lead', json.dumps(item)))
            actor = self.rt.checked_actor(db, 'lead')
            actor.update(accountKey='new-account', threadId='new-thread')
            self.rt.put(db, 'agents', actor)

    def test_native_output_survives_transfer_with_truncation_and_page_boundaries(self):
        first = self.rt.model_read('lead', {'output_ref': 'lead:exec-old'})
        second = self.rt.model_read('lead', {'output_ref': 'lead:exec-old', 'offset': first['nextOffset']})
        self.assertEqual(first['source'], 'saved_native_output')
        self.assertTrue(first['truncated'])
        self.assertEqual(first['outcome'], 'unknown')
        self.assertEqual(first['exitCode'], 0)
        self.assertEqual(first['nextOffset'], 3000)
        self.assertEqual(second['offset'], 3000)
        self.assertEqual(first['text'] + second['text'], ('Verified output\n' * 1000)[:6000])
        absent = self.rt.model_read('lead', {'output_ref': 'lead:exec-old', 'contains': 'missing'})
        self.assertTrue(absent['truncated'])
        with self.assertRaisesRegex(ValueError, 'not owned'):
            self.rt.model_read('worker', {'output_ref': 'lead:exec-old'})
        with self.rt.db() as db:
            db.execute("UPDATE runtime_items SET agent='worker' WHERE id='lead:exec-old'")
        with self.assertRaisesRegex(ValueError, 'not owned'):
            self.rt.model_read('lead', {'output_ref': 'lead:exec-old'})

    def test_incomplete_saved_json_reports_truncation_without_repeating_command(self):
        with self.rt.db() as db:
            db.execute("UPDATE runtime_items SET record=json_set(record,'$.text',?) WHERE id='lead:exec-old'", ('{"aggregatedOutput":',))
        with self.assertRaisesRegex(ValueError, 'truncated or unreadable'):
            self.rt.model_read('lead', {'output_ref': 'lead:exec-old'})


class RuntimeValidation(unittest.TestCase):
    def test_full_native_output_is_read_from_index_after_transcript_clips_json(self):
        module = fixture('runtime-contract')
        case = module.RuntimeContract()
        case.setUp()
        try:
            rt, lead = case.runtime, case.lead()
            output = 'Full command evidence 🙂\n' * 2500
            payload = {'id': 'exec-large', 'type': 'commandExecution', 'status': 'completed',
                       'exitCode': 0, 'aggregatedOutput': output}
            with rt.lock, rt.db() as db:
                rt.item(db, lead['id'], 'exec-large', 'output', json.dumps(payload), 'commandExecution')
                saved = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?',
                                              (lead['id'] + ':exec-large',)).fetchone()[0])
                self.assertTrue(saved['truncated'])
                with self.assertRaises(ValueError):
                    json.loads(saved['text'])
            chunks, offset = [], 0
            while True:
                found = rt.model_read(lead['id'], {'output_ref': lead['id'] + ':exec-large', 'offset': offset})
                self.assertFalse(found['truncated'])
                chunks.append(found['text'])
                if found['nextOffset'] is None:
                    break
                offset = found['nextOffset']
            self.assertEqual(''.join(chunks), output)
            self.assertEqual(found['outcome'], 'unknown')
            with rt.lock, rt.db() as db:
                db.execute('UPDATE runtime_search SET agent=? WHERE id=?', ('foreign', lead['id'] + ':exec-large'))
            with self.assertRaisesRegex(ValueError, 'truncated or unreadable'):
                rt.model_read(lead['id'], {'output_ref': lead['id'] + ':exec-large'})
        finally:
            case.tearDown()

    def test_stream_tail_and_completion_keep_source_truncation(self):
        module = fixture('runtime-contract')
        case = module.RuntimeContract()
        case.setUp()
        try:
            rt, lead = case.runtime, case.lead()
            scope = {'threadId': lead['threadId'], 'turnId': lead.get('turnId')}
            rt.notification({'method': 'item/started', 'params': {**scope, 'item': {
                'type': 'commandExecution', 'id': 'stream-tail', 'status': 'inProgress'}}})
            rt.notification({'method': 'item/commandExecution/outputDelta', 'params': {
                **scope, 'itemId': 'stream-tail', 'delta': 'x' * 14000}})
            rt.notification({'method': 'item/completed', 'params': {**scope, 'item': {
                'type': 'commandExecution', 'id': 'stream-tail', 'status': 'completed', 'exitCode': 0}}})
            found = rt.model_read(lead['id'], {'output_ref': lead['id'] + ':stream-tail'})
            self.assertTrue(found['truncated'])
            self.assertEqual(found['totalChars'], 12000)
        finally:
            case.tearDown()

    def test_dynamic_native_item_reference_resolves_same_agent_receipt_after_transfer(self):
        module = fixture('runtime-contract')
        case = module.RuntimeContract()
        case.setUp()
        try:
            rt, lead = case.runtime, case.lead()
            message = {'id': 9970, 'params': {'threadId': lead['threadId'], 'callId': 'historic-dynamic',
                       'tool': 'orchestration_peers', 'arguments': {'limit': 2}}}
            rt.dynamic(message)
            with rt.lock, rt.db() as db:
                rt.item(db, lead['id'], 'historic-dynamic', 'output', json.dumps({
                    'id': 'historic-dynamic', 'type': 'dynamicToolCall'}), 'dynamicToolCall')
                actor = rt.agent(lead['id'], db)
                actor.update(threadId='after-transfer', accountKey='after-transfer')
                rt.put(db, 'agents', actor)
            found = rt.model_read(lead['id'], {'output_ref': lead['id'] + ':historic-dynamic'})
            self.assertEqual(found['outputRef'], rt.tool_request_key(message))
            self.assertTrue(found['success'])
            self.assertEqual(found['outcome'], 'applied')
        finally:
            case.tearDown()

    def test_send_stable_identity_survives_lost_reply_and_workspace_bridge(self):
        from unittest.mock import patch
        module = fixture('runtime-contract')
        case = module.RuntimeContract()
        case.setUp()
        try:
            rt, lead = case.runtime, case.lead()
            worker = rt.create({'name': 'Send identity', 'prompt': 'Wait', 'role': 'reviewer'}, lead['id'], defer=True)
            args = {'agent_id': worker['id'], 'text': 'Continue exact task', 'request_id': 'send-once'}
            first = {'id': 9960, 'params': {'threadId': lead['threadId'], 'callId': 'send-first',
                     'tool': 'orchestration_send', 'arguments': args}}
            with patch.object(rt, 'reply', side_effect=ConnectionError('Lost tool response')):
                rt.dynamic(first)
            key = rt.tool_request_key(first)
            self.assertTrue(key.endswith(':send:send-once'))
            before = rt.tool_request(key)
            self.assertEqual(before['outcome'], 'applied')
            second = {'id': 9961, 'params': {**first['params'], 'callId': 'send-retry'}}
            rt.dynamic(second)
            bridge = {'id': 9962, 'params': {**first['params'], 'callId': 'send-bridge', 'arguments': {
                'agent_id': 'workspace', 'text': json.dumps({'tool': 'orchestration_send', 'arguments': args})}}}
            rt.dynamic(bridge)
            self.assertEqual(rt.tool_request_key(bridge), key)
            self.assertEqual(rt.tool_request(key), before)
            for call in ['send-first', 'send-retry', 'send-bridge', 'send-once']:
                self.assertEqual(rt.request_action(lead['id'], {'action': 'get', 'request_id': call})['id'], key)
            with rt.db() as db:
                self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events WHERE id=?', (key,)).fetchone()[0], 1)
            changed = {'id': 9963, 'params': {**first['params'], 'callId': 'send-changed',
                       'arguments': {**args, 'text': 'Different task'}}}
            with self.assertRaisesRegex(ValueError, 'different content'):
                rt.reserve_tool_request(changed)
            self.assertEqual(rt.tool_request(key), before)
            bridge_args = {**args, 'request_id': 'bridge-first'}
            fresh_bridge = {'id': 9964, 'params': {**first['params'], 'callId': 'fresh-bridge', 'arguments': {
                'agent_id': 'workspace', 'text': json.dumps({'tool': 'orchestration_send', 'arguments': bridge_args})}}}
            rt.dynamic(fresh_bridge)
            bridge_key = rt.tool_request_key(fresh_bridge)
            self.assertEqual(rt.tool_request(bridge_key)['outcome'], 'applied')
            rt.dynamic({'id': 9965, 'params': {**first['params'], 'callId': 'fresh-direct', 'arguments': bridge_args}})
            with rt.db() as db:
                self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events WHERE id=?', (bridge_key,)).fetchone()[0], 1)
        finally:
            case.tearDown()

    def test_invalid_native_tool_arguments_are_not_applied_and_retry_keeps_identity(self):
        module = fixture('runtime-contract')
        case = module.RuntimeContract()
        case.setUp()
        try:
            rt = case.runtime
            lead = case.lead()
            for number, tool, arguments in [(9971, 'orchestration_send', {}),
                                              (9972, 'orchestration_peers', {'limit': 51})]:
                message = {'id': number, 'params': {'threadId': lead['threadId'], 'callId': str(number),
                           'tool': tool, 'arguments': arguments}}
                rt.dynamic(message)
                key = rt.tool_request_key(message)
                before = rt.tool_request(key)
                self.assertFalse(before['result']['success'])
                self.assertEqual(before['outcome'], 'not_applied')
                rt.dynamic(message)
                self.assertEqual(rt.tool_request(key), before)
                self.assertFalse(rt.begin_tool_request(key))
        finally:
            case.tearDown()


class LegacyHTTP(unittest.TestCase):
    setUp = legacy.LegacyRecovery.setUp
    tearDown = legacy.LegacyRecovery.tearDown
    result = legacy.LegacyRecovery.result

    def test_old_server_recovery_uses_ledger_owner_and_historical_prefix_without_writes(self):
        with sqlite3.connect(self.database) as db:
            actor = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='lead'").fetchone()[0])
            actor.update(threadId='new', accountKey='new', accountHistory=[{'accountKey': 'default', 'threadId': 'thread-main'}])
            db.execute("UPDATE runtime_agents SET record=? WHERE id='lead'", (json.dumps(actor),))
            db.execute('CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY,record TEXT)')
            record = {'id': 'thread-main:bad-limit', 'agent': 'lead', 'tool': 'orchestration_peers',
                      'stage': 'failed', 'outcome': 'unknown', 'result': {'success': False,
                      'contentItems': [{'type': 'inputText', 'text': 'limit must be 1 to 50'}]}}
            db.execute('INSERT INTO runtime_tool_requests VALUES (?,?)', (record['id'], json.dumps(record)))
        before = self.database.read_bytes()
        for ref in ['thread-main:bad-limit', 'bad-limit']:
            found = recover_legacy_requests(self.url, 'lead', ref)
            self.assertEqual(found['outcome'], 'not_applied')
        self.assertEqual(recover_legacy_requests(self.url, 'lead', 'thread-main:ok')['outcome'], 'applied')
        self.assertEqual(recover_legacy_requests(self.url, 'other', record['id'])['stage'], 'not_found')
        self.assertEqual(self.database.read_bytes(), before)


if __name__ == '__main__':
    unittest.main()
