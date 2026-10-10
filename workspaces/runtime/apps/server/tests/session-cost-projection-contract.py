#!/usr/bin/env python3
"""Reuse the compact token projection without changing cost inputs or prices."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sqlite3
import statistics
import sys
import tempfile
import time
import unittest
from contextlib import closing, nullcontext
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_session_costs as costs
spec = importlib.util.spec_from_file_location('pricing_fixture', Path(__file__).with_name('pricing-session-cost-contract.py'))
pricing = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pricing)

# Original f29293c0 SQL is local, so source archives need no Git history.
ORIGINAL_PROJECTION = r"""
          CREATE TEMP TABLE session_cost_usage AS
            SELECT seq,agent,thread,turn,at,
                   json_extract(record,'$.agentId') AS record_agent,
                   json_extract(record,'$.threadId') AS record_thread,
                   json_extract(record,'$.turnId') AS record_turn,
                   CASE WHEN json_type(record,'$.responseId') IS NULL
                       OR json_type(record,'$.responseId') IN ('null','false')
                       OR (json_type(record,'$.responseId') IN ('integer','real') AND json_extract(record,'$.responseId')=0)
                       OR (json_type(record,'$.responseId')='text' AND json_extract(record,'$.responseId')='')
                       OR (json_type(record,'$.responseId')='array' AND json_array_length(record,'$.responseId')=0)
                       OR (json_type(record,'$.responseId')='object' AND NOT EXISTS
                           (SELECT 1 FROM json_each(analytics_usage.record,'$.responseId')))
                      THEN 0 ELSE 1 END AS has_response,
                   CASE WHEN json_type(record,'$.model')='text' THEN json_extract(record,'$.model') END AS model,
                   COALESCE(json_extract(record,'$.accountKey'),'default') AS account_key,
                   CASE WHEN json_type(record,'$.delta.inputTokens') IN ('integer','real','true','false')
                          AND json_type(record,'$.delta.outputTokens') IN ('integer','real','true','false')
                        THEN CASE WHEN json_type(record,'$.delta.inputTokens') IN ('integer','real') THEN json_extract(record,'$.delta.inputTokens') END
                        WHEN json_type(record,'$.last.inputTokens') IN ('integer','real') THEN json_extract(record,'$.last.inputTokens') END AS input_tokens,
                   CASE WHEN json_type(record,'$.delta.inputTokens') IN ('integer','real','true','false')
                          AND json_type(record,'$.delta.outputTokens') IN ('integer','real','true','false')
                        THEN CASE WHEN json_type(record,'$.delta.cachedInputTokens') IN ('integer','real') THEN json_extract(record,'$.delta.cachedInputTokens') END
                        WHEN json_type(record,'$.last.cachedInputTokens') IN ('integer','real') THEN json_extract(record,'$.last.cachedInputTokens') END AS cached_tokens,
                   CASE WHEN json_type(record,'$.delta.inputTokens') IN ('integer','real','true','false')
                          AND json_type(record,'$.delta.outputTokens') IN ('integer','real','true','false')
                        THEN CASE WHEN json_type(record,'$.delta.cacheWriteInputTokens') IN ('integer','real') THEN json_extract(record,'$.delta.cacheWriteInputTokens') END
                        WHEN json_type(record,'$.last.cacheWriteInputTokens') IN ('integer','real') THEN json_extract(record,'$.last.cacheWriteInputTokens') END AS write_tokens,
                   CASE WHEN json_type(record,'$.delta.inputTokens') IN ('integer','real','true','false')
                          AND json_type(record,'$.delta.outputTokens') IN ('integer','real','true','false')
                        THEN CASE WHEN json_type(record,'$.delta.outputTokens') IN ('integer','real') THEN json_extract(record,'$.delta.outputTokens') END
                        WHEN json_type(record,'$.last.outputTokens') IN ('integer','real') THEN json_extract(record,'$.last.outputTokens') END AS output_tokens,
                   CASE WHEN json_extract(record,'$.inputTokensAreUncached')=1 THEN 1 ELSE 0 END AS input_uncached
              FROM analytics_usage
             WHERE root=? AND NOT EXISTS
                   (SELECT 1 FROM session_cost_claude_messages l
                     WHERE l.account_key IS json_extract(analytics_usage.record,'$.accountKey')
                       AND l.thread_id IS analytics_usage.thread
                       AND l.response_id IS json_extract(analytics_usage.record,'$.responseId'))
        """


class OriginalProjection:
    """Freeze only the f29293c0 projection, not pricing or later grouping."""
    def __init__(self, db):
        self.db = db

    def execute(self, sql, parameters=()):
        if 'CREATE TEMP TABLE session_cost_usage AS' in sql:
            sql = ORIGINAL_PROJECTION
        return self.db.execute(sql, parameters)

    def commit(self):
        return self.db.commit()


class OriginalReader(costs.SessionCostReader):
    def _cost_usage_groups(self, db, root):
        return super()._cost_usage_groups(OriginalProjection(db), root)


def create_fixture(path, count=660, *, ordinary=False, padding=0):
    with closing(sqlite3.connect(path)) as db, db:
        db.executescript('''
          CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
          CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
          CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
          CREATE INDEX usage_root ON analytics_usage(root,seq);
        ''')
        agent = {'rootId': 'lead', 'accountKey': 'claude-account',
                 'provider': 'claude', 'threadId': 'thread'}
        db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps(agent),))
        responses = [None, False, 0, '', [], {}, 'receipt', True, 2, [0], {'x': 0}]
        models = [None, '', 'gpt-6-luna', 'claude-opus-5-5', 1, 'unknown-model']
        for index in range(count):
            turn = 'turn-' + str(index // 3)
            record = {
                'agentId': 'lead' if index % 4 else None,
                'threadId': 'thread' if index % 5 else None,
                'turnId': turn if index % 7 else None,
                'model': models[index % len(models)],
                'responseId': responses[index % len(responses)],
                'accountKey': ('claude-account', 'other-account', None)[index % 3],
                'inputTokensAreUncached': index % 2 == 0,
                'delta': {'inputTokens': True if index % 9 == 0 else 1000 + index % 3,
                          'outputTokens': 100, 'cachedInputTokens': 20,
                          'cacheWriteInputTokens': 30},
                'last': {'inputTokens': 20, 'outputTokens': 4,
                         'cachedInputTokens': 0, 'cacheWriteInputTokens': 0}}
            if index % 17 == 0:
                record['delta'].pop('outputTokens')
            if index % 11 == 0:
                record.pop('delta')
            if ordinary:
                record.update(agentId='lead', threadId='thread', turnId=turn,
                              model='gpt-6-luna', responseId='response-' + str(index))
                record['delta'] = {'inputTokens': 1000 + index % 3, 'outputTokens': 100,
                                   'cachedInputTokens': 20, 'cacheWriteInputTokens': 30}
            if padding:
                record['unrelated'] = 'x' * padding
            db.execute('INSERT INTO analytics_usage VALUES(?,?,?,?,?,?,?)',
                       (index + 1, 'lead', 'other-root' if index % 19 == 0 else 'lead',
                        'thread', turn, index, json.dumps(record)))
        # Preserve model fallback, correction signs, context tiers and nullable tokens.
        explicit = [
            {'model': 'gpt-6-luna', 'delta': {'inputTokens': -500, 'cachedInputTokens': -20,
                                            'cacheWriteInputTokens': 0, 'outputTokens': -2}},
            {'model': 'gpt-6-luna', 'inputTokensAreUncached': True,
             'delta': {'inputTokens': 272_000, 'cachedInputTokens': 20,
                       'cacheWriteInputTokens': 30, 'outputTokens': 1}},
            {'model': 'gpt-6-luna', 'delta': {'inputTokens': 272_000, 'cachedInputTokens': 0,
                                            'cacheWriteInputTokens': 0, 'outputTokens': 1}},
            {'model': 'claude-opus-5-5', 'delta': {'inputTokens': 100, 'cachedInputTokens': 20,
                                                'outputTokens': 4}},
            {'model': '<synthetic>', 'delta': {'inputTokens': 0, 'outputTokens': 0}},
            {'model': 'gpt-6-luna', 'delta': {'inputTokens': '100', 'outputTokens': 4},
             'last': {'inputTokens': 123.5, 'cachedInputTokens': 0,
                      'cacheWriteInputTokens': 0, 'outputTokens': 3.5}},
            {'model': 'gpt-6-luna', 'delta': {'inputTokens': True, 'cachedInputTokens': 1,
                                            'cacheWriteInputTokens': False, 'outputTokens': 4}},
            {'model': 'gpt-6-luna', 'delta': {'inputTokens': 1000, 'cachedInputTokens': 0,
                                            'cacheWriteInputTokens': 0, 'outputTokens': 1},
             'turnId': 'fallback'},
            {'model': None, 'delta': {'inputTokens': 2000, 'cachedInputTokens': 0,
                                     'cacheWriteInputTokens': 0, 'outputTokens': 2},
             'turnId': 'fallback'},
        ]
        for offset, record in enumerate(explicit):
            seq = count + offset + 1
            record = {'agentId': 'explicit', 'threadId': 'thread',
                      'turnId': 'explicit-' + str(offset), 'responseId': 'explicit-' + str(offset),
                      'accountKey': 'other-account', **record}
            db.execute('INSERT INTO analytics_usage VALUES(?,?,?,?,?,?,?)',
                       (seq, 'lead', 'lead', 'thread', 'explicit-' + str(offset), seq, json.dumps(record)))


def groups(path, *, original=False, legacy=False, steps=False):
    statements, step_count = [], [0]
    with closing(sqlite3.connect(path)) as db:
        db.execute('PRAGMA temp_store=FILE')
        db.execute('CREATE TEMP TABLE session_cost_claude_messages(account_key TEXT,thread_id TEXT,response_id TEXT,PRIMARY KEY(account_key,thread_id,response_id))')
        db.execute("INSERT INTO session_cost_claude_messages VALUES('claude-account','thread','receipt')")
        db.commit()
        db.execute('BEGIN')
        db.set_trace_callback(statements.append)
        if legacy:
            def reject_jsonb(action, first, second, _database, _trigger):
                return sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_FUNCTION and second in ('jsonb', 'jsonb_extract') else sqlite3.SQLITE_OK
            db.set_authorizer(reject_jsonb)
        if steps:
            def progress():
                step_count[0] += 1
                return 0
            db.set_progress_handler(progress, 1)
        reader_db = OriginalProjection(db) if original else db
        with patch.object(costs.sqlite3, 'sqlite_version_info', (3, 44, 0)) if legacy else nullcontext():
            result = list(costs.SessionCostReader._cost_usage_groups(None, reader_db, 'lead'))
        return sorted(result, key=repr), statements, step_count[0]


class SessionCostProjectionContract(unittest.TestCase):
    def test_grouped_values_preserve_original_projection(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'canvas.sqlite3'
            create_fixture(path, padding=4096)
            expected, _, _ = groups(path, original=True)
            actual, statements, _ = groups(path)
            self.assertEqual(actual, expected)
            self.assertTrue(any(row[2] == -500 and row[3] == -20 and row[5] == -2 for row in actual))
            self.assertTrue(any(row[0] == 'gpt-6-luna' and row[2] == 2000 for row in actual), 'The turn model must still fill the missing model.')
            self.assertTrue(any(row[2] == 123.5 and row[5] == 3.5 for row in actual), 'An invalid delta must still select last.')
            self.assertTrue(any(row[2] is None and row[4] is None for row in actual), 'Boolean tokens must not become numeric tokens.')
            self.assertEqual(sum('FROM analytics_usage' in sql and 'record' in sql for sql in statements), 1)

    def test_old_sqlite_uses_text_without_jsonb(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'canvas.sqlite3'
            create_fixture(path)
            expected, _, _ = groups(path, original=True)
            actual, statements, _ = groups(path, legacy=True)
            self.assertEqual(actual, expected)
            self.assertFalse(any('jsonb' in sql for sql in statements))

    def test_prices_receipt_exclusions_and_tiers_remain_exact(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            path = root / 'canvas.sqlite3'
            create_fixture(path)
            profile = root / 'claude'
            project = profile / 'projects' / 'sample'
            project.mkdir(parents=True)
            (project / 'thread.jsonl').write_text(json.dumps({
                'type': 'assistant', 'timestamp': '2026-10-04T12:00:00Z',
                'message': {'id': 'receipt', 'model': 'claude-opus-5-5', 'usage': {
                    'input_tokens': 100, 'cache_read_input_tokens': 20,
                    'cache_creation_input_tokens': 30, 'output_tokens': 10}}}) + '\n')
            accounts = {'claude-account': {'provider': 'claude', 'home': str(profile)}}
            expected = OriginalReader(path, pricing.FixedPricing(), accounts)._compute('lead', 'lead')
            actual = costs.SessionCostReader(path, pricing.FixedPricing(), accounts)._compute('lead', 'lead')
            self.assertEqual(actual, expected)
            self.assertGreater(actual['totalUSD'], 1)
            self.assertGreater(actual['pricedSamples'], 200)
            self.assertIn('unknown-model', actual['unknownModels'])
            self.assertIn('openai', actual['breakdown']['providers'])
            self.assertIn('anthropic', actual['breakdown']['providers'])

    def test_delta_and_last_containers_keep_original_outcome(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'canvas.sqlite3'
            create_fixture(path, 0)
            containers = ['abc', '{"inputTokens":150,"outputTokens":1}',
                          [{"inputTokens":150,"outputTokens":1}], 150, True, False, None,
                          {'inputTokens':150,'cachedInputTokens':0,'cacheWriteInputTokens':0,'outputTokens':1}]
            with closing(sqlite3.connect(path)) as db, db:
                db.execute('DELETE FROM analytics_usage')
                for delta_index, delta in enumerate(containers):
                    for last_index, last in enumerate(containers):
                        seq = delta_index * len(containers) + last_index + 1
                        record = {'model':'gpt-6-luna','responseId':str(seq),
                                  'delta':delta,'last':last}
                        db.execute('INSERT INTO analytics_usage VALUES(?,?,?,?,?,?,?)',
                                   (seq,'lead','lead','thread',str(seq),seq,json.dumps(record)))
            expected, _, _ = groups(path, original=True)
            self.assertEqual(groups(path)[0], expected)
            self.assertEqual(groups(path, legacy=True)[0], expected)
            self.assertEqual(sum(row[-1] for row in expected if row[2] is None), 49)

    def test_empty_root_and_malformed_record_keep_original_outcome(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'canvas.sqlite3'
            create_fixture(path, 0)
            with closing(sqlite3.connect(path)) as db, db:
                db.execute('DELETE FROM analytics_usage')
            self.assertEqual(groups(path)[0], [])
            self.assertEqual(groups(path, original=True)[0], [])
            with closing(sqlite3.connect(path)) as db, db:
                db.execute("INSERT INTO analytics_usage VALUES(1,'lead','lead','thread','turn',1,'broken-json')")
            for original in (False, True):
                with self.assertRaisesRegex(sqlite3.OperationalError, 'malformed JSON'):
                    groups(path, original=original)


def benchmark():
    """Report CPU time and SQLite VM steps without a timing gate in CI."""
    with tempfile.TemporaryDirectory() as folder:
        for padding in (0, 4096):
            path = Path(folder) / ('padded.db' if padding else 'compact.db')
            create_fixture(path, 30_000, ordinary=True, padding=padding)
            results, times = {}, {False: [], True: []}
            for _ in range(3):
                for original in (True, False):
                    started = time.process_time()
                    result, _, _ = groups(path, original=original)
                    times[original].append(time.process_time() - started)
                    results[original] = result
            assert results[False] == results[True]
            steps = {original: groups(path, original=original, steps=True)[2] for original in (True, False)}
            print(json.dumps({'rows': 30_000, 'padding': padding, 'sqlite': sqlite3.sqlite_version,
                              'baselineCPU': statistics.median(times[True]),
                              'candidateCPU': statistics.median(times[False]),
                              'baselineSteps': steps[True], 'candidateSteps': steps[False]}), flush=True)


if __name__ == '__main__':
    if sys.argv[1:] == ['--benchmark']:
        benchmark()
    else:
        unittest.main()
