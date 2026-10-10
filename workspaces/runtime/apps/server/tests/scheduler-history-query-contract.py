#!/usr/bin/env python3
"""Scheduler ticks skip terminal transfer and rule payloads, with fresh receipts."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_account_transfer as transfer
import codex_rules as rules

spec = importlib.util.spec_from_file_location('rule_fixture', Path(__file__).with_name('rules-owner-cache-cpu-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class HistoryQueryContract(unittest.TestCase):
    def test_terminal_transfer_payloads_are_never_decoded(self):
        db = sqlite3.connect(':memory:')
        self.addCleanup(db.close)
        db.execute('CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY,record TEXT)')
        db.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)')
        db.execute("CREATE INDEX runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'))")
        for index in range(200):
            record = {'id': str(index), 'status': 'completed' if index % 2 else 'cancelled', 'payload': 'x' * 16384}
            db.execute('INSERT INTO runtime_account_transfers VALUES(?,?)', (record['id'], json.dumps(record)))
        for identity in ('first', 'second'):
            record = {'id': identity, 'status': 'pending', 'members': {}}
            db.execute('INSERT INTO runtime_account_transfers VALUES(?,?)', (identity, json.dumps(record)))
        db.commit()
        @contextmanager
        def database():
            with db:
                yield db
        runtime = SimpleNamespace(lock=threading.RLock(), db=database,
            records=lambda *_args: self.fail('A tick must not read transfer history'))
        store = transfer.AccountTransfers.__new__(transfer.AccountTransfers)
        store.rt, store.closing = runtime, False
        store.running, store.futures = set(), {}
        saved = []
        def save(_db, record):
            saved.append(record['id'])
            _db.execute('UPDATE runtime_account_transfers SET record=? WHERE id=?',
                        (json.dumps(record), record['id']))
        store.save = save
        decoded = []
        original = json.loads
        def load(raw):
            record = original(raw)
            decoded.append(record['id'])
            return record
        with patch.object(transfer.json, 'loads', side_effect=load):
            store.tick([])
        self.assertEqual(decoded, ['first', 'second'])
        self.assertEqual(saved, ['first', 'second'])
        self.assertEqual(original(db.execute("SELECT record FROM runtime_account_transfers WHERE id='first'").fetchone()[0])['status'], 'cancelled')

    def test_inactive_rules_do_not_decode_or_read_their_removed_owners(self):
        import tempfile
        with tempfile.TemporaryDirectory(prefix='studio-rule-history-') as directory:
            runtime = fixture.RuleFixture(directory)
            runtime.add_owner()
            for index in range(200):
                runtime.add_rule('history-' + str(index), 'removed-owner', status='paused', payload='x' * 16384)
            runtime.add_rule('active', nextAt=0)
            runtime.add_rule('busy', inFlight=True)
            runtime.records = lambda *_args: self.fail('A tick must not load full rule history')
            runtime.rules_tick()
            self.assertEqual([(phase, key) for phase, key, _ in runtime.owner_loads],
                             [('read', 'owner'), ('writer', 'owner')])
            self.assertEqual([rule['id'] for rule in runtime.pool.launched], ['active'])


if __name__ == '__main__':
    unittest.main()
