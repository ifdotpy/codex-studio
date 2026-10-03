#!/usr/bin/env python3
"""Compact cost rows preserve totals and reduce full JSON history scans."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from contextlib import closing

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
if os.environ.get('STUDIO_COST_SOURCE_DIR'):
    sys.path.insert(0, os.environ['STUDIO_COST_SOURCE_DIR'])
import codex_session_costs
from codex_source import source_function
spec = importlib.util.spec_from_file_location('pricing_fixture', Path(__file__).with_name('pricing-session-cost-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class SessionCostScanContract(unittest.TestCase):
    def test_compact_history_preserves_legacy_group_rules(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'canvas.sqlite3'
            with closing(sqlite3.connect(path)) as db, db:
                db.executescript('''
                  CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
                  CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
                  CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
                  CREATE INDEX usage_root ON analytics_usage(root,seq);
                ''')
                db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({'rootId': 'lead'}),))
                responses = [None, False, 0, '', [], {}, 'response', True, 2, [0], {'x': 0}]
                models = [None, '', 'gpt-6-luna', 'claude-opus-5-5', 1, 'unknown-model']
                for index in range(660):
                    record = {'agentId': 'lead' if index % 4 else None,
                              'threadId': 'thread' if index % 5 else None,
                              'turnId': 'turn-' + str(index // 3) if index % 7 else None,
                              'model': models[index % len(models)],
                              'responseId': responses[index % len(responses)],
                              'accountKey': 'account' if index % 3 else None,
                              'inputTokensAreUncached': index % 2 == 0 and index % 9 != 0,
                              'delta': {'inputTokens': True if index % 9 == 0 else 1000 + index % 3,
                                        'outputTokens': 100, 'cachedInputTokens': 20, 'cacheWriteInputTokens': 30},
                              'last': {'inputTokens': 20, 'outputTokens': 4,
                                       'cachedInputTokens': 0, 'cacheWriteInputTokens': 0},
                              'unrelated': 'x' * 4096}
                    if index % 11 == 0:
                        record.pop('delta')
                    db.execute('INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)',
                               (index + 1, 'lead', 'lead', 'thread', 'turn-' + str(index // 3), index, json.dumps(record)))
            reader = codex_session_costs.SessionCostReader(path, f.FixedPricing())
            statements = []
            connect = reader._connect
            def traced():
                db = connect()
                db.set_trace_callback(statements.append)
                return db
            reader._connect = traced
            result = reader._compute('lead', 'lead')
            self.assertEqual(result['pricedSamples'], 249)
            self.assertAlmostEqual(result['totalUSD'], .19585445)
            self.assertAlmostEqual(result['breakdown']['providers']['openai'], .02482845)
            self.assertAlmostEqual(result['breakdown']['providers']['anthropic'], .171026)
            self.assertEqual(result['unknownModels'],
                             ['Unknown model', 'claude-opus-5-5', 'gpt-6-luna', 'unknown-model'])
            scans = [sql for sql in statements if 'FROM analytics_usage' in sql and 'record' in sql]
            self.assertLessEqual(len(scans), 2, 'JSON history is read only for log exclusion and the compact projection')
            self.assertEqual(sum('CREATE TEMP TABLE session_cost_usage' in sql for sql in statements), 1)
            # Reuse a frozen baseline only when supplied for a paired source check.
            baseline = os.environ.get('STUDIO_COST_BASELINE')
            if baseline:
                old, _ = source_function(Path(baseline).read_bytes(), ('SessionCostReader', '_compute'), vars(codex_session_costs))
                marker = len(statements)
                expected = old(reader, 'lead', 'lead')
                self.assertEqual(result, expected)
                old_scans = [sql for sql in statements[marker:] if 'FROM analytics_usage' in sql and 'record' in sql]
                self.assertGreater(len(old_scans), len(scans))
                print('Paired source totals match; JSON scans:', len(old_scans), 'to', len(scans))


if __name__ == '__main__':
    unittest.main()
