#!/usr/bin/env python3
"""Turn rate, provider counts, and zero added SQLite writes. No inference."""
import contextlib
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_token_rate import TokenRates, TurnRate
from codex_runtime import Runtime
from codex_streaming import StreamBuffer
spec = importlib.util.spec_from_file_location('runtime_fixture', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class RateContract(unittest.TestCase):
    def test_window_smooths_bursts_and_decays_during_tool_wait(self):
        rate = TurnRate('one', 0)
        for now in (.5, 1, 1.5, 2):
            rate.text('x' * 40, now)
        self.assertEqual(rate.snapshot(2)['rate'], 20)
        self.assertEqual(rate.snapshot(4)['rate'], 10)
        self.assertEqual(rate.snapshot(7)['rate'], 0)
        self.assertLessEqual(len(rate.bins), 17)

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

    def test_usage_without_text_spreads_output_over_its_interval(self):
        rate = TurnRate('one', 0)
        rate.correct(80, 8)
        self.assertEqual(rate.snapshot(8)['rate'], 10)
        rate.finish(8)
        self.assertEqual(rate.snapshot(80)['rate'], 10)
        self.assertFalse(rate.snapshot(80)['active'])

    def test_fast_usage_receipt_keeps_its_full_output_count(self):
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
        self.assertEqual(rate.snapshot(.01)['outputTokens'], 0)
        self.assertEqual(rate.snapshot(.01)['rate'], 0)
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
                    def traced_db():
                        with original_db() as db:
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
