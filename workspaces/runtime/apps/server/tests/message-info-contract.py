#!/usr/bin/env python3
"""Message metadata reads preserve exact source identities and never write state."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import sys
import unittest
from contextlib import contextmanager
from unittest.mock import patch
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_token_rate import TokenRates

spec = importlib.util.spec_from_file_location('analytics_fixture', Path(__file__).with_name('analytics-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class MessageInfoContract(unittest.TestCase):
    setUp = fixture.AnalyticsContract.setUp
    tearDown = fixture.AnalyticsContract.tearDown
    event = fixture.AnalyticsContract.event
    data = fixture.AnalyticsContract.data

    def seed(self, identified=True):
        self.event('turn/started', {}, at=100)
        self.event('item/completed', {'item': {'id': 'answer', 'type': 'agentMessage', 'text': 'Answer'}}, at=101)
        self.event('thread/tokenUsage/updated', {**({'responseId': 'response-one'} if identified else {}),
                   'requestUsage': {'outputTokens': 40, 'reasoningOutputTokens': 10},
                   'rawTokenUsageRecord': {'response_id': 'response-one'},
                   'tokenUsage': {'last': {'outputTokens': 40, 'reasoningOutputTokens': 10}, 'total': {'totalTokens': 80}}}, at=102)
        self.event('turn/completed', {'turn': {'id': self.agent['turnId'], 'status': 'completed'}, 'durationMs': 2500}, at=103)

    def info(self, **options):
        return self.data(view='message-info', item='answer', turn=self.agent['turnId'], **options)

    def test_read_only_exact_turn_metadata_and_response_usage(self):
        self.seed(identified=False)
        with self.runtime.analytics_read_connection() as db:
            before = list(db.execute('SELECT id,record FROM analytics_turns'))
        rates = TokenRates(lambda: 0)
        actor = {'id': self.agent['id'], 'threadId': self.agent['threadId'], 'inFlight': True}
        rates.observe(actor, 'turn/started', {'turn': {'id': self.agent['turnId']}},
                      'default', 'connection', 100)
        rates.observe(actor, 'thread/tokenUsage/updated', {
            'turnId': self.agent['turnId'],
            'tokenUsage': {'last': {'outputTokens': 40}, 'total': {'outputTokens': 40}},
        }, 'default', 'connection', 102)
        self.runtime._token_rates = rates
        self.event('thread/tokenUsage/updated', {'responseId': 'response-one',
            'requestUsage': {'outputTokens': 40, 'reasoningOutputTokens': 10},
            'rawTokenUsageRecord': {'response_id': 'response-one'},
            'tokenUsage': {'last': {'outputTokens': 40, 'reasoningOutputTokens': 10},
                           'total': {'totalTokens': 80, 'outputTokens': 40}}}, at=102, source='rollout')
        original_read = self.runtime.analytics_read_connection
        @contextmanager
        def read_only():
            with original_read() as db:
                yield db
                self.assertEqual(db.total_changes, 0)
        with patch.object(self.runtime, 'analytics_read_connection', read_only), patch.object(self.runtime.accounts, 'get', side_effect=AssertionError('No credential refresh')):
            result = self.info()
        self.assertEqual(result['model'], self.agent['model'])
        self.assertEqual(result['effort'], self.agent['effort'])
        self.assertEqual(result['accountKey'], self.agent.get('accountKey', 'default'))
        self.assertEqual(result['provider'], 'codex')
        self.assertEqual(result['accountLabel'], 'Codex')
        self.assertEqual(result['turnDurationMs'], 2500)
        self.assertEqual(result['tokens'], {'outputTokens': 40, 'reasoningOutputTokens': 10})
        self.assertEqual(self.info()['responseRate'], 20)
        self.assertEqual(result['at'], 101)
        with self.runtime.analytics_read_connection() as db:
            self.assertEqual(before, list(db.execute('SELECT id,record FROM analytics_turns')))
            self.assertEqual(db.total_changes, 0)
        self.assertEqual(self.info(thread='unrelated-thread'), {})

    def test_multiple_responses_or_messages_omit_unattributed_counts(self):
        self.seed()
        self.event('item/completed', {'item': {'id': 'commentary', 'type': 'agentMessage', 'text': 'Update'}}, at=104)
        self.assertNotIn('tokens', self.info())
        self.event('thread/tokenUsage/updated', {'responseId': 'response-two',
                   'tokenUsage': {'last': {'outputTokens': 20}, 'total': {'totalTokens': 100}}}, at=105)
        self.assertNotIn('tokens', self.info())

    def test_tool_response_and_provisional_usage_never_supply_message_tokens(self):
        self.seed()
        self.event('item/completed', {'item': {'id': 'tool', 'type': 'commandExecution', 'command': 'check'}}, at=104)
        self.assertNotIn('tokens', self.info())
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("DELETE FROM analytics_items WHERE json_extract(record,'$.itemId')='tool'")
        self.event('thread/tokenUsage/updated', {'tokenUsage': {'last': {'outputTokens': 900}, 'total': {'totalTokens': 1000}}}, at=105)
        self.assertNotIn('tokens', self.info())

    def test_duplicate_native_ids_do_not_mix_threads_or_current_model(self):
        self.seed()
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(self.agent['id'], db)
            current['model'] = 'changed-model'
            self.runtime.put(db, 'agents', current)
        self.assertEqual(self.info()['model'], self.agent['model'])
        self.event('item/completed', {'threadId': 'other-thread', 'item': {'id': 'answer', 'type': 'agentMessage', 'text': 'Other'}}, at=106)
        self.event('turn/completed', {'threadId': 'other-thread', 'turn': {'id': self.agent['turnId'], 'status': 'completed'}}, at=107)
        self.assertEqual(self.info(), {})
        self.assertEqual(self.info(thread=self.agent['threadId'])['tokens']['outputTokens'], 40)
        with self.assertRaises(ValueError):
            self.data(view='message-info', item='answer', scope='team')


if __name__ == '__main__':
    unittest.main()
