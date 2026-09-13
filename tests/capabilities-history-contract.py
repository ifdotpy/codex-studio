#!/usr/bin/env python3
"""Capability discovery reads scoped names and preserves independent provider results."""
from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_workspace import WorkspaceMixin


class Inventory(WorkspaceMixin):
    def __init__(self, path, call):
        self.path = path
        self.capability_cache = {}
        self.call = call

    def checked_actor_in_own_db(self, key):
        return {'id': key, 'model': 'fixture', 'role': 'lead', 'cwd': '/fixture', 'threadId': 'thread'}

    @staticmethod
    def tool_definitions():
        return [{'name': 'fixture-tool'}]

    def records(self, *_args):
        raise AssertionError('Capability discovery must not load full task records')

    @property
    def lock(self):
        raise AssertionError('Capability reads must not hold the shared runtime lock')

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path)
        try:
            yield db
        finally:
            db.close()

    def connect(self, *_args):
        return self


class CapabilityHistoryContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'history.sqlite3'
        with sqlite3.connect(self.path) as db:
            db.executescript("""
                CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'), json_extract(record,'$.created'));
                CREATE INDEX runtime_task_history ON runtime_tasks(json_extract(record,'$.created') DESC, json_extract(record,'$.agent')) WHERE json_extract(record,'$.status')!='running';
            """)
        self.calls = []
        def call(method, params, timeout):
            self.calls.append((method, params, timeout))
            return {'data': [method], 'nextCursor': 'next-' + method}
        self.inventory = Inventory(self.path, call)

    def insert(self, *records):
        with sqlite3.connect(self.path) as db:
            db.executemany('INSERT INTO runtime_tasks VALUES (?,?)', [(str(index), json.dumps(record)) for index, record in enumerate(records)])

    def test_all_history_statuses_fallbacks_and_selected_agent_names(self):
        selected = [
            {'agent': 'lead', 'status': 'completed', 'name': 'old-tool', 'created': 1},
            {'agent': 'lead', 'status': 'running', 'name': 'current-tool', 'created': 2},
            {'agent': 'lead', 'status': 'failed', 'type': 'failed-type', 'created': 3},
            {'agent': 'lead', 'status': 'cancelled', 'name': 'old-tool', 'created': 4},
            {'agent': 'lead', 'name': 'missing-status', 'created': 5},
            {'agent': 'lead', 'status': None, 'name': 'null-status', 'created': 6},
            {'agent': 'lead', 'status': 'completed', 'created': 7},
        ]
        foreign = [{'agent': 'other', 'status': 'completed', 'name': 'unselected',
                    'created': index + 8, 'arguments': 'x' * 100000} for index in range(20)]
        self.insert(*selected, *foreign)
        result = self.inventory.capabilities('lead')
        expected = sorted({row.get('name', row.get('type', '')) for row in selected})
        self.assertEqual(result['observed'], expected)
        self.assertEqual(result['observedNative'], expected)
        self.assertEqual(result['managed'], [{'name': 'fixture-tool'}])
        self.assertEqual(result['skills'], ['skills/list'])
        self.assertEqual(result['servers'], ['mcpServerStatus/list'])
        self.assertEqual(result['errors'], [])
        self.assertTrue(all(timeout == 5 for _, _, timeout in self.calls))
        self.assertIs(self.inventory.capabilities('lead'), result)
        self.assertEqual(len(self.calls), 2)

    def test_provider_reads_overlap_and_failure_preserves_other_inventory(self):
        entered = threading.Barrier(2)
        def call(method, params, timeout):
            self.assertEqual(timeout, 5)
            entered.wait(timeout=2)
            if method == 'skills/list':
                raise TimeoutError('fixture deadline')
            return {'servers': [{'name': 'available'}], 'nextCursor': 'next'}
        self.inventory.call = call
        result = self.inventory.capabilities('lead')
        self.assertEqual(result['errors'], ['skills: fixture deadline'])
        self.assertEqual(result['servers'], [{'name': 'available'}])
        self.assertEqual(result['mcp'], result['servers'])
        self.assertEqual(result['serversCursor'], 'next')
        self.assertEqual(result['skills'], [])


if __name__ == '__main__':
    unittest.main()
