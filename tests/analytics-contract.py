#!/usr/bin/env python3
"""Analytics capture uses isolated real Runtime state and a deterministic native server."""
import base64
import importlib.util
import json
from pathlib import Path
import sqlite3
import struct
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('runtime_contract', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import Runtime
from codex_analytics import payload_size


class AnalyticsContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.agent = self.runtime.create({'name': 'Analytics lead', 'cwd': self.temp.name, 'prompt': 'Wait'})
        fixture.eventually(lambda: self.runtime.agent(self.agent['id'])['status'] == 'running')
        self.agent = self.runtime.agent(self.agent['id'])

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def event(self, method, p, at=100, source='live'):
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.analytics_event(db, self.agent, method, {'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'], **p}, at=at, source=source)

    def usage(self, total, last, at=100, **p):
        self.event('thread/tokenUsage/updated', {'tokenUsage': {'total': total, 'last': last, 'modelContextWindow': 10000}, **p}, at=at)

    def data(self, **options):
        return self.runtime.analytics(self.agent['id'], **options)

    def test_usage_duplicate_equal_sized_requests_and_subsets(self):
        one = {'inputTokens': 80, 'cachedInputTokens': 60, 'cacheWriteInputTokens': 5, 'outputTokens': 20, 'reasoningOutputTokens': 10, 'totalTokens': 100}
        self.usage(one, one)
        self.usage(one, one)
        self.usage({k: v * 2 for k, v in one.items()}, one, at=101)
        result = self.data()
        self.assertEqual(result['summary']['usageSamples'], 2)
        self.assertEqual(result['summary']['tokens']['totalTokens'], 200)
        self.assertEqual(result['summary']['tokens']['reasoningOutputTokens'], 20)
        self.assertEqual(result['summary']['cacheHitRate'], .75)
        self.assertEqual(result['summary']['peakContextPercent'], 1)

    def test_first_lifetime_counter_missing_and_resets_are_not_spend(self):
        self.usage({'totalTokens': 9000}, {'totalTokens': 100})
        self.usage({'totalTokens': 20}, {'totalTokens': 20}, at=101)
        self.assertEqual(self.data()['summary']['tokens']['totalTokens'], 120)
        self.assertIsNone(self.data()['summary']['tokens']['cachedInputTokens'])
        self.assertTrue(self.data()['timeline'][1]['reset'])

    def test_history_enriches_live_usage_without_double_count_optional_fields(self):
        self.usage({'totalTokens': 100}, {'totalTokens': 100})
        self.event('thread/tokenUsage/updated', {'responseId': 'response-1', 'tokenUsage': {'total': {'totalTokens': 100, 'cacheWriteInputTokens': 0}, 'last': {'totalTokens': 100, 'inputTokens': 80}, 'modelContextWindow': None}, 'rawTokenUsageRecord': {'response_id': 'response-1'}}, at=99, source='rollout')
        result = self.data()
        self.assertEqual(result['summary']['usageSamples'], 1)
        self.assertEqual(result['timeline'][0]['responseId'], 'response-1')
        self.assertEqual(result['summary']['tokens']['inputTokens'], 80)
        self.assertEqual(result['summary']['tokens']['totalTokens'], 100)

    def test_distinct_zero_token_response_ids_remain_distinct(self):
        for response in ('one', 'two', 'two'):
            self.usage({'totalTokens': 100}, {'totalTokens': 0}, responseId=response)
        self.assertEqual(self.data()['summary']['usageSamples'], 2)

    def test_partial_cache_measurements_do_not_imply_zero(self):
        self.usage({'totalTokens': 100}, {'totalTokens': 100, 'inputTokens': 100, 'cachedInputTokens': 80})
        self.usage({'totalTokens': 1000}, {'totalTokens': 900, 'inputTokens': 900}, at=101)
        summary = self.data()['summary']
        self.assertIsNone(summary['cacheHitRate'])
        self.assertEqual(summary['cacheHitRateSamples'], 1)
        self.assertEqual(summary['cacheHitRateTotalSamples'], 2)

    def test_global_importer_errors_visible_for_agent_scope(self):
        with self.runtime.db() as db:
            db.execute('INSERT INTO analytics_history VALUES (?,?,?)', ('importer', '', json.dumps({'status': 'error', 'error': 'Importer failed'})))
        self.assertEqual(self.data()['coverage']['historyErrors'][0]['status'], 'error')
        self.assertEqual(self.data()['history'][0]['status'], 'error')

    def test_current_agent_metadata_overlays_historical_snapshot(self):
        with self.runtime.db() as db:
            self.runtime.analytics_agent(db, {**self.agent, 'model': 'old-model', 'effort': 'low'})
        self.assertEqual(self.data()['agents'][0]['model'], self.agent['model'])

    def test_native_restart_counter_does_not_duplicate_exact_response(self):
        self.usage({'totalTokens': 134249}, {'totalTokens': 134249}, at=100)
        self.event('thread/tokenUsage/updated', {'responseId': 'actual', 'rawTokenUsageRecord': {'response_id': 'actual'},
            'tokenUsage': {'total': {'totalTokens': 52104576}, 'last': {'totalTokens': 134249}}}, at=99.5, source='rollout')
        self.event('thread/tokenUsage/updated', {'responseId': 'actual',
            'tokenUsage': {'total': {'totalTokens': 134249}, 'last': {'totalTokens': 134249}}}, at=100, source='rollout')
        result = self.data()
        self.assertEqual(result['summary']['usageSamples'], 1)
        self.assertEqual(result['summary']['tokens']['totalTokens'], 134249)
        self.assertEqual(result['timeline'][0]['total']['totalTokens'], 52104576)
        self.assertEqual(result['timeline'][0]['noticeTotal']['totalTokens'], 134249)
        self.assertEqual(result['summary']['provisionalUsageSamples'], 1)
        # A range excluding the exact timestamp cannot recategorize its duplicate.
        self.assertIsNone(self.data(**{'from': 99.8})['summary']['tokens']['totalTokens'])

    def test_legacy_only_turn_keeps_usage_with_other_exact_turn_present(self):
        self.usage({'totalTokens': 200}, {'totalTokens': 20})
        self.event('thread/tokenUsage/updated', {'turnId': 'another-turn', 'responseId': 'known',
            'tokenUsage': {'total': {'totalTokens': 240}, 'last': {'totalTokens': 40}}}, at=101)
        self.assertEqual(self.data()['summary']['tokens']['totalTokens'], 60)
        self.assertEqual(self.data()['summary']['legacyUsageSamples'], 1)

    def test_compaction_snapshot_and_native_item_count_once(self):
        self.event('analytics/compaction', {'id': 'snapshot', 'window_number': 4})
        self.event('item/completed', {'item': {'id': 'native-compact', 'type': 'contextCompaction'}}, at=100.004)
        result = self.data()
        self.assertEqual(result['summary']['compactions'], 1)
        self.assertEqual(len(result['compactionSnapshots']), 1)
        self.assertEqual(result['summary']['toolCalls'], 0)

    def test_namespace_and_compaction_replacement_are_not_new_tool_calls(self):
        with self.runtime.db() as db:
            self.runtime.analytics_model_payload(db, self.agent, {'type': 'function_call', 'namespace': 'notes', 'name': 'read', 'call_id': 'model', 'arguments': '{}'}, at=100)
            self.runtime.analytics_model_payload(db, self.agent, {'type': 'function_call', 'name': 'read', 'call_id': 'replacement', 'arguments': '{}', '_analyticsCategory': 'compactionReplacement'}, at=100)
        result = self.data()
        self.assertEqual(result['summary']['modelToolCalls'], 1)
        self.assertEqual(result['calls'][0]['name'], 'notes.read')

    def test_compaction_replacement_snapshots_preserve_each_measurement(self):
        with self.runtime.db() as db:
            for at, amount in ((100, 100), (200, 200)):
                self.runtime.analytics_model_payload(db, self.agent, {'type': 'function_call_output', 'call_id': 'old-call', 'output': 'x' * amount, '_analyticsCategory': 'compactionReplacement', '_analyticsId': 'snapshot-' + str(at)}, at=at)
        with self.runtime.db() as db:
            rows = [json.loads(r[0]) for r in db.execute("SELECT record FROM analytics_items WHERE type='modelMessage'")]
        self.assertEqual(len(rows), 2)
        self.assertEqual([r['output']['bytes'] for r in sorted(rows, key=lambda r: r['at'])], [100, 200])
        self.assertEqual(self.data()['summary']['modelToolCalls'], 0)

    def test_empty_message_start_is_not_first_observed_output(self):
        self.event('turn/started', {'turnId': 'output', 'turn': {'id': 'output'}}, at=100)
        self.event('item/started', {'turnId': 'output', 'item': {'id': 'message-empty', 'type': 'agentMessage', 'text': ''}}, at=101)
        self.event('item/agentMessage/delta', {'turnId': 'output', 'itemId': 'message-empty', 'delta': 'Hello'}, at=103)
        turn = next(t for t in self.data()['turns'] if t['turnId'] == 'output')
        self.assertEqual(turn['firstOutputDelayMs'], 3000)

    def test_output_captured_before_ui_truncation_and_stream_not_retained(self):
        item = {'id': 'tool-large', 'type': 'commandExecution', 'command': 'cat data'}
        self.event('item/started', {'item': item})
        self.event('item/commandExecution/outputDelta', {'itemId': item['id'], 'delta': 'z' * 50000}, at=101)
        output = 'я\n' * 50000
        self.event('item/completed', {'item': {**item, 'aggregatedOutput': output, 'durationMs': 1500, 'exitCode': 0}}, at=102)
        call = self.data()['calls'][0]
        self.assertEqual(call['output']['bytes'], len(output.encode()))
        self.assertEqual(call['output']['chars'], len(output))
        self.assertEqual(call['stream']['bytes'], 50000)
        self.assertEqual(call['durationMs'], 1500)
        with self.runtime.db() as db:
            stored = db.execute('SELECT record FROM analytics_items').fetchone()[0]
        self.assertNotIn('z' * 100, stored)
        self.assertNotIn('я' * 100, stored)

    def test_model_payload_and_native_stdout_are_separate(self):
        self.event('item/completed', {'item': {'id': 'native', 'type': 'commandExecution', 'command': 'echo hello', 'aggregatedOutput': 'hello'}})
        with self.runtime.db() as db:
            for p in ({'type': 'function_call', 'call_id': 'wrapper', 'name': 'functions.exec', 'arguments': 'code'}, {'type': 'function_call_output', 'call_id': 'wrapper', 'output': 'formatted result hello'}):
                self.runtime.analytics_model_payload(db, self.agent, p, at=100, turn_id=self.agent['turnId'])
        result = self.data()
        self.assertEqual(result['summary']['protocolOutputBytes'], 5)
        self.assertEqual(result['summary']['modelOutputBytes'], len('formatted result hello'))
        self.assertEqual(result['summary']['outputBytes'], len('formatted result hello'))
        self.assertEqual(result['summary']['modelToolCalls'], 1)
        self.assertEqual(result['summary']['protocolToolCalls'], 1)
        self.assertEqual(result['summary']['toolCalls'], 1)
        self.assertEqual(result['summary']['observedToolRows'], 2)

    def test_tool_filter_does_not_attribute_provider_usage(self):
        self.usage({'totalTokens': 100}, {'totalTokens': 100})
        for name in ('one', 'two'):
            self.event('item/completed', {'item': {'id': name, 'type': 'dynamicToolCall', 'tool': name, 'arguments': {}, 'contentItems': []}})
        result = self.data(tool='one')
        self.assertEqual(len(result['calls']), 1)
        self.assertEqual(len(result['tools']), 2)
        self.assertEqual(result['summary']['tokens']['totalTokens'], 100)

    def test_exact_image_sizes_and_unknown_remote_image_size(self):
        png = b'\x89PNG\r\n\x1a\n' + b'12345678' + struct.pack('>II', 1000, 150)
        value = payload_size([{'type': 'inputImage', 'imageUrl': 'data:image/png;base64,' + base64.b64encode(png).decode()}])
        self.assertEqual(value['imageCount'], 1)
        self.assertEqual(value['imageBytes'], len(png))
        self.assertEqual(value['images'][0]['width'], 1000)
        self.assertIsNone(payload_size({'type': 'image', 'url': 'https://example.invalid/image'})['imageBytes'])

    def test_dynamic_result_and_notification_deduplicate(self):
        p = {'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'], 'callId': 'dynamic', 'tool': 'orchestration_peers', 'arguments': {}}
        self.runtime.dynamic({'id': 'request-1', 'params': p})
        response = self.runtime.server.responses[-1]['result']
        self.event('item/completed', {'item': {'id': 'dynamic', 'type': 'dynamicToolCall', 'tool': 'orchestration_peers', **response}})
        self.assertEqual(self.data()['summary']['protocolToolCalls'], 1)
        self.assertGreater(self.data()['calls'][0]['output']['bytes'], 0)

    def test_failed_capture_does_not_block_native_completion(self):
        with patch.object(self.runtime, 'analytics_event', side_effect=ValueError('bad analytics')):
            self.runtime.server.complete(self.agent['threadId'], self.agent['turnId'])
        self.assertEqual(self.runtime.agent(self.agent['id'])['status'], 'completed')
        self.assertGreater(self.data()['coverage']['captureErrors']['count'], 0)

    def test_failed_dynamic_capture_does_not_consume_tool_reply(self):
        with patch.object(self.runtime, 'analytics_dynamic', side_effect=ValueError('bad analytics')):
            self.runtime.dynamic({'id': 'reply-safe', 'params': {'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'], 'callId': 'safe', 'tool': 'orchestration_peers', 'arguments': {}}})
        self.assertTrue(self.runtime.server.responses[-1]['result']['success'])
        self.assertEqual(self.data()['coverage']['captureErrors']['count'], 1)

    def test_close_keeps_runtime_lease_until_history_batch_finishes(self):
        entered, release, finished = threading.Event(), threading.Event(), threading.Event()
        errors = []
        def delayed_step():
            entered.set()
            if not release.wait(5):
                raise AssertionError('The test did not release the history batch')
            # The real importer reacquires this lock to commit its checkpoint.
            with self.runtime.lock, self.runtime.db() as db:
                db.execute("INSERT INTO analytics_meta VALUES ('lastBatch','done')")
            finished.set()
            return False
        def close_runtime():
            try:
                self.runtime.close()
            except Exception as error:
                errors.append(error)
        with patch.object(self.runtime, 'analytics_history_step', side_effect=delayed_step):
            self.runtime.analytics_history_start()
            self.assertTrue(entered.wait(2))
            closer = threading.Thread(target=close_runtime)
            closer.start()
            try:
                fixture.eventually(lambda: self.runtime.closed)
                self.assertTrue(closer.is_alive())
                with self.assertRaisesRegex(RuntimeError, 'Another canvas runtime owns'):
                    Runtime(Path(self.temp.name), fixture.FakeServer)
            finally:
                release.set()
                closer.join(3)
            self.assertFalse(closer.is_alive())
            self.assertFalse(self.runtime.analytics_history_thread.is_alive())
            self.assertTrue(finished.is_set())
            self.assertEqual(errors, [])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT value FROM analytics_meta WHERE key='lastBatch'").fetchone()[0], 'done')
        # A new owner is admitted only after the old batch has committed.
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)

    def test_history_completed_call_filters_by_known_start(self):
        self.event('item/completed', {'startedAt': 100, 'completedAt': 200,
            'item': {'id': 'historical-span', 'type': 'commandExecution', 'command': 'build', 'aggregatedOutput': 'done'}}, at=200, source='rollout')
        calls = self.data(**{'from': 90, 'to': 110})['calls']
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]['at'], 100)
        self.assertEqual(calls[0]['finishedAt'], 200)
        self.assertEqual(self.data(**{'from': 190, 'to': 210})['calls'], [])

    def test_history_enrichment_updates_live_call_indexed_start(self):
        item = {'id': 'late-start', 'type': 'commandExecution', 'command': 'build', 'aggregatedOutput': 'done'}
        self.event('item/completed', {'item': item}, at=200)
        self.assertEqual(len(self.data(**{'from': 190, 'to': 210})['calls']), 1)
        self.event('item/completed', {'item': item, 'startedAt': 100, 'completedAt': 200}, at=200, source='rollout')
        call = self.data(**{'from': 90, 'to': 110})['calls'][0]
        self.assertEqual(call['at'], 100)
        self.assertEqual(call['firstSourceAt'], 200)
        self.assertEqual(call['source'], 'live')
        self.assertEqual(self.data(**{'from': 190, 'to': 210})['calls'], [])

    def test_model_call_output_before_input_uses_input_start_for_filters(self):
        with self.runtime.db() as db:
            self.runtime.analytics_model_payload(db, self.agent,
                {'type': 'function_call_output', 'call_id': 'unordered', 'output': 'done'}, at=200)
            self.runtime.analytics_model_payload(db, self.agent,
                {'type': 'function_call', 'call_id': 'unordered', 'name': 'exec', 'arguments': 'build'}, at=100)
        call = self.data(**{'from': 90, 'to': 110})['calls'][0]
        self.assertEqual(call['at'], 100)
        self.assertEqual(call['finishedAt'], 200)
        self.assertEqual(call['durationMs'], 100000)
        self.assertEqual(self.data(**{'from': 190, 'to': 210})['calls'], [])

    def test_export_pagination_time_and_restart_retention(self):
        for index in range(3):
            self.event('item/completed', {'item': {'id': str(index), 'type': 'webSearch', 'query': str(index)}}, at=100 + index)
        self.assertEqual(len(self.data(limit=1)['calls']), 1)
        self.assertEqual(len(self.data(limit=1, export=1)['calls']), 3)
        self.assertEqual(self.data(**{'from': 101, 'to': 102})['pagination']['total'], 2)
        self.runtime.close()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.assertEqual(self.data()['pagination']['total'], 3)

    def test_turn_wall_time_and_unknown_measurements(self):
        self.event('turn/started', {'turnId': 'measured', 'turn': {'id': 'measured'}}, at=100)
        self.event('item/agentMessage/delta', {'turnId': 'measured', 'itemId': 'msg', 'delta': 'Hi'}, at=102)
        self.event('turn/completed', {'turnId': 'measured', 'turn': {'id': 'measured', 'status': 'completed'}}, at=105)
        turn = next(t for t in self.data()['turns'] if t['turnId'] == 'measured')
        self.assertEqual(turn['durationMs'], 5000)
        self.assertEqual(turn['firstOutputDelayMs'], 2000)


if __name__ == '__main__':
    unittest.main()
