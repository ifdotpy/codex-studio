#!/usr/bin/env python3
"""Turn rate, provider counts, and zero added SQLite writes. No inference."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import contextlib
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_token_rate import TokenRates, TurnRate, event_time
from codex_runtime import Runtime
from codex_streaming import StreamBuffer
spec = importlib.util.spec_from_file_location('runtime_fixture', SERVER_TESTS_ROOT / 'runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class RateContract(unittest.TestCase):
    def test_codex_rollout_usage_after_tool_result_uses_generation_span(self):
        # rollout: token_usage_record precedes a tool call; token_count arrives after its result.
        rates = TokenRates(lambda: 100.1)
        agent = {'id': 'luna', 'threadId': 'native', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        rates.observe(agent, 'item/started', {'turnId': 'turn',
            'item': {'id': 'tool', 'type': 'dynamicToolCall'}}, 'a', 'c', 10)
        rates.observe(agent, 'item/completed', {'turnId': 'turn',
            'item': {'id': 'tool', 'type': 'dynamicToolCall'}}, 'a', 'c', 100)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'turn',
            'tokenUsage': {'total': {'inputTokens': 110000, 'cachedInputTokens': 108000,
                                     'outputTokens': 320},
                           'last': {'inputTokens': 110000, 'cachedInputTokens': 108000,
                                    'outputTokens': 320, 'reasoningOutputTokens': 120}}},
            'a', 'c', 100.1)
        sample = rates.snapshot('luna')
        self.assertEqual(sample['rate'], 32)
        self.assertEqual(sample['outputTokens'], 320)
        self.assertFalse(rates.snapshot('luna')['estimated'])

    def test_batched_codex_receipts_keep_response_order_and_time(self):
        rates = TokenRates(lambda: 30.2)
        agent = {'id': 'sol', 'threadId': 'native', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        for item_id, start, finish in (('first', 10, 20), ('second', 28, 30)):
            item = {'id': item_id, 'type': 'commandExecution'}
            rates.observe(agent, 'item/started', {'turnId': 'turn', 'item': item}, 'a', 'c', start)
            rates.observe(agent, 'item/completed', {'turnId': 'turn', 'item': item}, 'a', 'c', finish)
        for at, total, last in ((30.1, 320, 320), (30.2, 480, 160)):
            rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'turn',
                'tokenUsage': {'total': {'outputTokens': total},
                               'last': {'outputTokens': last}}}, 'a', 'c', at)
        self.assertEqual(rates.snapshot('sol')['rate'], 20)
        self.assertEqual(rates.snapshot('sol')['outputTokens'], 480)

    def test_late_usage_preserves_next_response_stream_estimate(self):
        rate = TurnRate('turn', 0)
        rate.text('x' * 40, 1)
        rate.start_tool('exec', 2)
        self.assertEqual(rate.snapshot(2)['outputTokens'], 10)
        rate.finish_tool('exec', 10)
        rate.text('x' * 80, 11)
        self.assertEqual(rate.snapshot(11)['outputTokens'], 30)
        rate.response(100, 11.1, 'previous-response')
        sample = rate.snapshot(11.1)
        self.assertEqual(sample['rate'], 20)
        self.assertTrue(sample['estimated'])
        self.assertEqual(sample['outputTokens'], 120)
        self.assertEqual(rate.messages['previous-response'][2], 50)

    def test_tool_wait_silence_and_completion_freeze_previous_rate(self):
        now = [2]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'sol', 'threadId': 'native', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        rates.stream('item/reasoning/textDelta', {'threadId': 'native', 'turnId': 'turn',
            'delta': 'x' * 400}, 'a', 'c', 2)
        first = rates.snapshot('sol')['rate']
        self.assertGreater(first, 0)
        rates.observe(agent, 'item/started', {'turnId': 'turn',
            'item': {'id': 'tool', 'type': 'dynamicToolCall'}}, 'a', 'c', 2)
        self.assertEqual(rates.snapshot('sol')['rate'], first)
        now[0] = 12
        self.assertEqual(rates.snapshot('sol')['rate'], first)
        rates.observe(agent, 'item/completed', {'turnId': 'turn',
            'item': {'id': 'tool', 'type': 'dynamicToolCall'}}, 'a', 'c', 20)
        now[0] = 20
        self.assertEqual(rates.snapshot('sol')['rate'], first)
        rates.stream('item/agentMessage/delta', {'threadId': 'native', 'turnId': 'turn',
            'delta': 'x' * 40}, 'a', 'c', 21)
        updated = rates.snapshot('sol')['rate']
        now[0] = 31
        self.assertEqual(rates.snapshot('sol')['rate'], updated)
        rates.observe(agent, 'turn/completed', {'turn': {'id': 'turn'}}, 'a', 'c', 32)
        self.assertEqual(rates.snapshot('sol')['rate'], updated)
        self.assertFalse(rates.snapshot('sol')['active'])

    def test_text_estimate_tracks_recent_stream_not_elapsed_turn(self):
        rate = TurnRate('turn', 0)
        rate.text('x' * 80, 1)
        rate.text('x' * 80, 101)
        self.assertEqual(rate.snapshot(101)['rate'], 20)
        self.assertEqual(rate.snapshot(111)['rate'], 20)

    def test_claude_response_usage_arrives_at_message_end(self):
        now = [0]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'opus', 'threadId': 'claude'}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        now[0] = 4
        self.assertEqual(rates.snapshot('opus')['rate'], 0)
        rates.observe(agent, 'item/completed', {'turnId': 'turn',
            'item': {'id': 'msg_1', 'type': 'agentMessage', 'text': 'Done'},
            'tokenRateUsage': {'responseId': 'msg_1', 'outputTokens': 400}}, 'a', 'c', 5)
        now[0] = 5
        self.assertEqual(rates.snapshot('opus')['rate'], 80)
        now[0] = 15
        self.assertEqual(rates.snapshot('opus')['rate'], 80)

    def test_claude_message_start_excludes_turn_queue_and_is_deduplicated(self):
        rates = TokenRates(lambda: 105)
        agent = {'id': 'opus', 'threadId': 'claude'}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        params = {'threadId': 'claude', 'turnId': 'turn', 'responseId': 'msg_1'}
        rates.stream('provider/generationStarted', params, 'a', 'c', 100)
        rates.stream('provider/generationStarted', params, 'a', 'c', 102)
        rates.observe(agent, 'item/completed', {'turnId': 'turn',
            'item': {'id': 'msg_1', 'type': 'agentMessage', 'text': 'Done'},
            'tokenRateUsage': {'responseId': 'msg_1', 'outputTokens': 400}}, 'a', 'c', 105)
        self.assertEqual(rates.snapshot('opus')['rate'], 80)

    def test_reasoning_heavy_response_uses_full_model_generation_time_and_holds(self):
        rates = TokenRates(lambda: 0)
        agent = {'id': 'worker', 'threadId': 'thread', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        rates.observe(agent, 'thread/tokenUsage/updated', {
            'turnId': 'turn', 'tokenUsage': {
                'total': {'outputTokens': 1409, 'reasoningOutputTokens': 1072},
                'last': {'outputTokens': 1409, 'reasoningOutputTokens': 1072},
            },
        }, 'a', 'c', 18.8)
        self.assertEqual(rates.snapshot('worker')['rate'], 74.95)
        self.assertEqual(rates.snapshot('worker')['outputTokens'], 1409)
        self.assertEqual(rates.snapshot('worker')['rate'], 74.95, 'The last response rate holds during gaps')

    def test_tool_and_user_input_spans_are_excluded(self):
        rates = TokenRates(lambda: 0)
        agent = {'id': 'worker', 'threadId': 'thread', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        spans = [('commandExecution', 2, 4, 200), ('mcpToolCall', 5, 8, 100),
                 ('dynamicToolCall', 10, 15, 200)]
        for index, (kind, started, completed, output) in enumerate(spans):
            item = {'id': 'tool-' + str(index), 'type': kind}
            rates.observe(agent, 'item/started', {'turnId': 'turn', 'item': item}, 'a', 'c', started)
            rates.observe(agent, 'item/completed', {'turnId': 'turn', 'item': item}, 'a', 'c', completed)
            rates.stream('provider/outputUsage', {'threadId': 'thread', 'turnId': 'turn',
                'responseId': 'r' + str(index), 'outputTokens': output}, 'a', 'c', completed)
        rates.request_started('a', 'c', 'thread', 'question', 20)
        rates.request_finished('a', 'c', 'question', 30)
        rates.stream('provider/outputUsage', {'threadId': 'thread', 'turnId': 'turn',
            'responseId': 'r3', 'outputTokens': 500}, 'a', 'c', 30)
        rates.stream('provider/outputUsage', {'threadId': 'thread', 'turnId': 'turn',
            'responseId': 'r4', 'outputTokens': 300}, 'a', 'c', 33)
        self.assertEqual(rates.snapshot('worker')['rate'], 100)
        self.assertEqual(rates.snapshot('worker')['outputTokens'], 1300)

    def test_receive_time_wins_over_queued_dispatch_time(self):
        message = {'_studioReceivedAt': 18.8, '_studioDispatchedAt': 50, 'params': {}}
        self.assertEqual(event_time(message, 'thread/tokenUsage/updated'), 18.8)
        self.assertIsNone(event_time({'_studioDispatchedAt': 50, 'params': {}}, 'thread/tokenUsage/updated'))
        self.assertEqual(event_time({**message, 'params': {'completedAt': 18.7}}, 'item/completed'), 18.7)
        rates = TokenRates(lambda: 0)
        agent = {'id': 'worker', 'threadId': 'thread', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        rates.observe(agent, 'thread/tokenUsage/updated', {
            'turnId': 'turn', 'tokenUsage': {'total': {'outputTokens': 940}, 'last': {'outputTokens': 940}},
        }, 'a', 'c', event_time(message, 'thread/tokenUsage/updated'))
        self.assertEqual(rates.snapshot('worker')['rate'], 50)

    def test_late_count_corrects_interval_without_receipt_spike(self):
        rate = TurnRate('one', 0)
        for now in (1, 2, 3, 4, 5, 6, 7, 8):
            rate.text('x' * 40, now)
        rate.correct(160, 8)
        self.assertEqual(rate.snapshot(8)['outputTokens'], 160)
        self.assertFalse(rate.snapshot(8)['estimated'])
        self.assertLessEqual(rate.snapshot(8)['rate'], 25)
        rate.text('x' * 40, 8.1)
        rate.correct(160, 8.2)  # A repeated receipt cannot erase later estimates.
        self.assertTrue(rate.snapshot(8.2)['estimated'])
        self.assertEqual(rate.snapshot(8.2)['outputTokens'], 170)

    def test_usage_without_text_uses_response_duration_and_finishes(self):
        rate = TurnRate('one', 0)
        rate.correct(80, 8)
        self.assertEqual(rate.snapshot(8)['rate'], 10)
        rate.finish(8)
        self.assertEqual(rate.snapshot(80)['rate'], 10)
        self.assertFalse(rate.snapshot(80)['active'])

    def test_fast_usage_receipt_keeps_count_and_applies_rate_guard(self):
        rate = TurnRate('one', 0)
        rate.correct(50, .01)
        self.assertEqual(rate.snapshot(.01)['rate'], 200)
        self.assertEqual(rate.snapshot(.01)['outputTokens'], 50)

    def test_implausible_correction_is_ignored_and_display_is_capped(self):
        rate = TurnRate('one', 0)
        with self.assertLogs('codex_token_rate', level='WARNING') as log:
            rate.correct(745, .01)
            TurnRate('two', 0).correct(745, .01)
        self.assertEqual(len(log.output), 1)
        self.assertEqual(rate.snapshot(.01)['outputTokens'], 745)
        self.assertEqual(rate.snapshot(.01)['rate'], 1000)
        rate.text('x' * 20000, .02)
        self.assertEqual(rate.snapshot(.02)['rate'], 1000)

    def test_codex_lifetime_baseline_current_turn_reset_and_stale_sources(self):
        now = [0]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'lead', 'threadId': 'thread', 'turnId': 'one', 'inFlight': True}
        def notice(method, **params):
            rates.observe(agent, method, params, 'account', 'connection')
        def usage(total, last, turn='one'):
            notice('thread/tokenUsage/updated', turnId=turn, tokenUsage={
                'total': {'inputTokens': 9000, 'outputTokens': total},
                'last': {'inputTokens': 8000, 'outputTokens': last, 'reasoningOutputTokens': 10}})
        notice('turn/started', turn={'id': 'one'})
        now[0] = 2
        usage(1050, 50)
        self.assertEqual(rates.snapshot('lead')['outputTokens'], 50)
        self.assertEqual(rates.snapshot('lead')['rate'], 25)
        usage(1050, 50)
        now[0] = 3
        usage(1080, 30)
        self.assertEqual(rates.snapshot('lead')['outputTokens'], 80)
        notice('turn/completed', turn={'id': 'one'})
        agent['turnId'] = 'two'
        now[0] = 4
        notice('turn/started', turn={'id': 'two'})
        self.assertEqual(rates.snapshot('lead')['outputTokens'], 0)
        self.assertEqual(rates.snapshot('lead')['rate'], 0)
        usage(1080, 80, turn='one')
        rates.stream('item/agentMessage/delta', {'threadId': 'thread', 'turnId': 'one', 'delta': 'x' * 400}, 'account', 'connection')
        rates.stream('item/agentMessage/delta', {'threadId': 'thread', 'turnId': 'two', 'delta': 'x' * 400}, 'account', 'old-connection')
        self.assertEqual(rates.snapshot('lead')['outputTokens'], 0)
        now[0] = 5
        usage(1100, 20, turn='two')
        self.assertEqual(rates.snapshot('lead')['outputTokens'], 20)

    def test_first_usage_after_resume_seeds_lifetime_baseline(self):
        now = [20]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'resumed', 'threadId': 'thread', 'turnId': 'active', 'inFlight': True}
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'active', 'tokenUsage': {
            'total': {'outputTokens': 100000}, 'last': {'outputTokens': 120}}}, 'a', 'new-connection')
        self.assertEqual(rates.snapshot('resumed')['outputTokens'], 120)
        now[0] = 22
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'active', 'tokenUsage': {
            'total': {'outputTokens': 100180}, 'last': {'outputTokens': 60}}}, 'a', 'new-connection')
        self.assertEqual(rates.snapshot('resumed')['outputTokens'], 300)
        self.assertEqual(rates.snapshot('resumed')['rate'], 0)
        self.assertFalse(rates.snapshot('resumed')['estimated'])

    def test_resume_without_interval_never_displays_capped_exact_rate(self):
        rates = TokenRates(lambda: 0)
        agent = {'id': 'resumed-exact', 'threadId': 'thread', 'turnId': 'turn', 'inFlight': True}
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'turn', 'tokenUsage': {
            'total': {'outputTokens': 100000}, 'last': {'outputTokens': 1141}}}, 'a', 'c', 100)
        sample = rates.snapshot('resumed-exact')
        self.assertEqual((sample['outputTokens'], sample['rate'], sample['estimated']), (1141, 0, False))
        rates = TokenRates(lambda: 0)
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 100)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'turn', 'tokenUsage': {
            'total': {'outputTokens': 100000}, 'last': {'outputTokens': 1141}}}, 'a', 'c', 122.59)
        self.assertEqual(rates.snapshot('resumed-exact')['rate'], 50.51)

    def test_claude_turn_total_does_not_replace_final_response_rate(self):
        rates = TokenRates(lambda: 0)
        agent = {'id': 'claude-correction', 'threadId': 'thread'}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        rates.stream('provider/outputUsage', {'threadId': 'thread', 'turnId': 'turn',
            'responseId': 'one', 'outputTokens': 8}, 'a', 'c', 1)
        rates.stream('provider/outputUsage', {'threadId': 'thread', 'turnId': 'turn',
            'responseId': 'two', 'outputTokens': 12}, 'a', 'c', 2)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'turn',
            'responseId': 'two', 'responseOutputTokens': 16, 'turnOutputTokens': 24,
            'tokenUsage': {'last': {'outputTokens': 16}}}, 'a', 'c', 2)
        self.assertEqual(rates.snapshot('claude-correction')['outputTokens'], 24)
        self.assertEqual(rates.snapshot('claude-correction')['rate'], 16)
        self.assertEqual(rates.response_rate('claude-correction', 'thread', 'turn', 'two')['outputTokens'], 16)

    def test_supervisor_receipt_time_wins_over_backend_replay_time(self):
        message = {'_studioSupervisorReceivedAt': 100.02, '_studioReceivedAt': 120,
                   'params': {'turnId': 't'}}
        self.assertEqual(event_time(message, 'thread/tokenUsage/updated'), 100.02)

    def test_native_counter_reset_starts_segment_and_keeps_turn_count(self):
        rates = TokenRates(lambda: 0)
        agent = {'id': 'reset', 'threadId': 'thread', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'old'}}, 'a', 'c', 0)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'old', 'tokenUsage': {
            'total': {'outputTokens': 10000}, 'last': {'outputTokens': 10000}}}, 'a', 'c', 1)
        rates.observe(agent, 'turn/started', {'turn': {'id': 'new'}}, 'a', 'c', 2)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'new', 'tokenUsage': {
            'total': {'outputTokens': 10100}, 'last': {'outputTokens': 100}}}, 'a', 'c', 3)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'new', 'tokenUsage': {
            'total': {'outputTokens': 200}, 'last': {'outputTokens': 200}}}, 'a', 'c', 7)
        self.assertEqual(rates.snapshot('reset')['outputTokens'], 300)
        self.assertEqual(rates.snapshot('reset')['rate'], 50)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'new', 'tokenUsage': {
            'total': {'outputTokens': 200}, 'last': {'outputTokens': 200}}}, 'a', 'c', 8)
        self.assertEqual(rates.snapshot('reset')['outputTokens'], 300)

    def test_unique_native_sample_can_link_exact_rollout_response_id(self):
        rates = TokenRates(lambda: 0)
        agent = {'id': 'codex', 'threadId': 'thread', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'turn', 'tokenUsage': {
            'total': {'outputTokens': 1141}, 'last': {'outputTokens': 1141}}}, 'a', 'c', 20)
        self.assertTrue(rates.associate_response_rate('codex', 'thread', 'turn', 'resp-1', 1141))
        self.assertEqual(rates.response_rate('codex', 'thread', 'turn', 'resp-1'), {
            'rate': 57.05, 'outputTokens': 1141, 'durationSeconds': 20})
        self.assertFalse(rates.associate_response_rate('codex', 'thread', 'turn', 'resp-2', 1141))

        ambiguous = TokenRates(lambda: 0)
        ambiguous.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c', 0)
        for at, total in ((20, 1141), (40, 2282)):
            ambiguous.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'turn', 'tokenUsage': {
                'total': {'outputTokens': total}, 'last': {'outputTokens': 1141}}}, 'a', 'c', at)
        self.assertFalse(ambiguous.associate_response_rate('codex', 'thread', 'turn', 'ambiguous', 1141))

    def test_claude_turn_output_corrections_are_cumulative(self):
        now = [0]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'claude', 'threadId': 'thread', 'turnId': 'one', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'one'}}, 'a', 'c')
        for now[0], cumulative in ((1, 40), (2, 100)):
            rates.observe(agent, 'thread/tokenUsage/updated', {
                'turnId': 'one', 'turnOutputTokens': cumulative,
                'tokenUsage': {'total': {'outputTokens': cumulative}, 'last': {'outputTokens': cumulative}},
            }, 'a', 'c')
        self.assertEqual(rates.snapshot('claude')['outputTokens'], 100)

    def test_missing_baseline_keeps_estimate_and_input_tokens_never_count(self):
        rates = TokenRates(lambda: 1)
        agent = {'id': 'worker', 'threadId': 'thread'}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'one'}}, 'a', 'c')
        params = {'threadId': 'thread', 'turnId': 'one', 'delta': 'x' * 40}
        rates.stream('item/agentMessage/delta', params, 'a', 'c')
        rates.observe(agent, 'thread/tokenUsage/updated', {'turnId': 'one', 'tokenUsage': {'total': {'outputTokens': 1000}}}, 'a', 'c')
        self.assertTrue(rates.snapshot('worker')['estimated'])
        self.assertEqual(rates.snapshot('worker')['outputTokens'], 10)
        rates.stream('item/commandExecution/outputDelta', params, 'a', 'c')
        self.assertEqual(rates.snapshot('worker')['outputTokens'], 10)

    def test_team_batch_keeps_each_worker_scoped_and_omits_idle_and_old_connections(self):
        now = [0]
        rates = TokenRates(lambda: now[0])
        agents = [
            {'id': 'lead', 'rootId': 'lead', 'threadId': 'lead-thread'},
            {'id': 'one', 'rootId': 'lead', 'threadId': 'one-thread'},
            {'id': 'two', 'rootId': 'lead', 'threadId': 'two-thread'},
            {'id': 'other', 'rootId': 'other-team', 'threadId': 'other-thread'},
        ]
        for index, agent in enumerate(agents):
            rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c')
            rates.stream('item/agentMessage/delta', {'threadId': agent['threadId'], 'turnId': 'turn', 'delta': 'x' * (index + 1) * 40}, 'a', 'c')
        now[0] = 2
        batch = rates.team_snapshot('lead')
        self.assertEqual(set(batch), {'one', 'two'})
        self.assertEqual(batch['one']['outputTokens'], 20)
        self.assertEqual(batch['two']['outputTokens'], 30)
        shared = rates.workspace_snapshot()
        self.assertEqual(set(shared['rates']), {'lead', 'one', 'two', 'other'})
        self.assertEqual(shared['teams']['lead'], batch)
        rates.observe(agents[1], 'turn/completed', {'turn': {'id': 'turn'}}, 'a', 'c')
        self.assertEqual(set(rates.team_snapshot('lead')), {'one', 'two'})
        self.assertFalse(rates.team_snapshot('lead')['one']['active'])
        rates.observe(agents[2], 'turn/started', {'turn': {'id': 'new-turn'}}, 'a', 'new-connection')
        self.assertEqual(rates.team_snapshot('lead')['two']['outputTokens'], 0)
        self.assertEqual(rates.team_snapshot('other-team')['other']['outputTokens'], 40)

    def test_rate_is_tracked_before_any_client_reads_a_snapshot(self):
        now = [0]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'unwatched', 'rootId': 'unwatched', 'threadId': 'thread', 'turnId': 'turn', 'inFlight': True}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'turn'}}, 'a', 'c')
        rates.stream('item/agentMessage/delta', {
            'threadId': 'thread', 'turnId': 'turn', 'delta': 'x' * 400,
        }, 'a', 'c')
        now[0] = 2
        self.assertEqual(rates.snapshot('unwatched')['outputTokens'], 100)

    def test_claude_message_counts_are_deduplicated_and_final_total_corrects(self):
        now = [0]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'worker', 'threadId': 'thread'}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'one'}}, 'a', 'c')
        def usage(**params):
            rates.stream('provider/outputUsage', {'threadId': 'thread', 'turnId': 'one', **params}, 'a', 'c')
        now[0] = 2
        usage(responseId='message-1', outputTokens=20)
        usage(responseId='message-1', outputTokens=20)
        usage(responseId='message-1', outputTokens=24)
        usage(responseId='message-2', outputTokens=30)
        self.assertEqual(rates.snapshot('worker')['outputTokens'], 54)
        usage(turnOutputTokens=60)
        self.assertEqual(rates.snapshot('worker')['outputTokens'], 60)
        self.assertFalse(rates.snapshot('worker')['estimated'])

    def test_claude_response_rate_uses_provider_output_count(self):
        now = [0]
        rates = TokenRates(lambda: now[0])
        agent = {'id': 'claude', 'threadId': 'thread'}
        rates.observe(agent, 'turn/started', {'turn': {'id': 'one'}}, 'a', 'c', 0)
        now[0] = 4
        rates.stream('provider/outputUsage', {
            'threadId': 'thread', 'turnId': 'one', 'responseId': 'claude-response', 'outputTokens': 160,
        }, 'a', 'c', now[0])
        self.assertEqual(rates.snapshot('claude')['outputTokens'], 160)
        self.assertEqual(rates.snapshot('claude')['rate'], 40)


class NoScheduleRuntime(Runtime):
    def schedule(self):
        pass


class WriteContract(unittest.TestCase):
    def test_streamed_turn_has_identical_writes_with_meter_enabled_or_disabled(self):
        measurements = []
        for enabled in (False, True):
            with tempfile.TemporaryDirectory() as directory:
                runtime = NoScheduleRuntime(Path(directory), fixture.FakeServer)
                try:
                    lead = runtime.create({'name': 'Lead', 'cwd': directory, 'prompt': 'Wait'}, defer=True)
                    with runtime.lock, runtime.db() as db:
                        lead.update(autoWake=True, status='waiting')
                        runtime.put(db, 'agents', lead)
                    agent = runtime.create({'name': 'Fixture', 'prompt': 'Wait'}, parent=lead['id'], defer=True)
                    with runtime.lock, runtime.db() as db:
                        agent.update(threadId='fixture-thread', autoWake=True, inFlight=True, status='running')
                        runtime.put(db, 'agents', agent)
                    rates = TokenRates()
                    runtime._token_rates = rates
                    writes = []
                    original_db = runtime.db
                    @contextlib.contextmanager
                    def traced_db(**kwargs):
                        with original_db(**kwargs) as db:
                            db.set_trace_callback(lambda sql: writes.append(sql.split()[0].upper()) if sql.split() and sql.split()[0].upper() in {'INSERT', 'UPDATE', 'DELETE', 'REPLACE'} else None)
                            yield db
                            db.set_trace_callback(None)
                    runtime.db = traced_db
                    def send(method, **params):
                        runtime.notification({'method': method, 'params': {'threadId': 'fixture-thread', 'turnId': 'one', **params}})
                    with contextlib.ExitStack() as stack:
                        stack.enter_context(patch.object(StreamBuffer, '_schedule_locked', return_value=None))
                        if not enabled:
                            stack.enter_context(patch.object(rates, 'observe'))
                            stack.enter_context(patch.object(rates, 'stream'))
                        send('turn/started', turn={'id': 'one'})
                        before_start = len(writes)
                        send('provider/generationStarted', responseId='fixture-response')
                        self.assertEqual(len(writes), before_start, 'Provider start writes nothing')
                        if enabled:
                            self.assertEqual(rates.entries[('default', None, 'fixture-thread')]
                                             ['rate'].generation_response_id, 'fixture-response')
                        send('item/started', item={'id': 'answer', 'type': 'agentMessage', 'text': ''})
                        for _ in range(2000):
                            send('item/agentMessage/delta', itemId='answer', delta='x' * 20)
                        send('item/completed', item={'id': 'answer', 'type': 'agentMessage', 'text': 'x' * 40000}, tokenRateUsage={'responseId': 'claude-message', 'outputTokens': 10000})
                        send('thread/tokenUsage/updated', tokenUsage={'total': {'totalTokens': 10200, 'outputTokens': 10000}, 'last': {'totalTokens': 10200, 'outputTokens': 10000}})
                        before = len(writes)
                        for _ in range(2000):
                            rates.snapshot(agent['id'])
                            rates.team_snapshot(agent['rootId'])
                            rates.workspace_snapshot()
                        self.assertEqual(len(writes), before, 'Meter reads write nothing')
                        if enabled:
                            self.assertEqual(rates.snapshot(agent['id'])['outputTokens'], 10000)
                            self.assertEqual(rates.team_snapshot(lead['id'])[agent['id']]['outputTokens'], 10000)
                        measurements.append(len(writes))
                finally:
                    runtime.close()
        self.assertEqual(*measurements)
        print({'deltas': 2000, 'meterDisabledWrites': measurements[0], 'meterEnabledWrites': measurements[1], 'addedWrites': 0})


if __name__ == '__main__':
    unittest.main()
