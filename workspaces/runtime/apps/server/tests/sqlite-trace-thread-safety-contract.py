#!/usr/bin/env python3
"""SQLite diagnostics use owner events without touching foreign connections."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import gc
import importlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import weakref

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_sqlite
import codex_sqlite_traces as traces


def mutex_case(root):
    """Hold the actual SQLite mutex in an authorizer while diagnostics run."""
    ready, release = threading.Event(), threading.Event()
    errors = []
    traces.SLOW_MS = 0
    def authorizer(action, *args):
        if action == sqlite3.SQLITE_SELECT:
            ready.set()
            if not release.wait(3):
                raise RuntimeError('The fixture authorizer timed out')
        return sqlite3.SQLITE_OK
    def owner():
        db = codex_sqlite.connect(':memory:', site='contract.prepare')
        try:
            db.execute('BEGIN')
            db.set_authorizer(authorizer)
            db.execute('SELECT 1').fetchall()
            db.set_authorizer(None)
            db.rollback()
        except BaseException as error:
            errors.append(error)
        finally:
            db.close()
    thread = threading.Thread(target=owner, name='contract-sqlite-owner', daemon=True)
    thread.start()
    try:
        assert ready.wait(1), 'The fixture did not hold the SQLite mutex'
        print('MUTEX_HELD', flush=True)
        active = traces.active_transactions()
        assert len(active) == 1, active
        assert active[0]['threadId'] == thread.ident
        assert 'authorizer' in {frame['function'] for frame in active[0]['frames']}
        diagnostics = codex_sqlite.diagnostics()
        assert len(diagnostics['activeTransactions']) == 1, diagnostics
        diagnostic = diagnostics['activeTransactions'][0]
        assert diagnostic['site'] == 'contract.prepare'
        assert diagnostic['threads'][0]['threadId'] == thread.ident
        assert 'authorizer' in {frame['function'] for frame in diagnostic['threads'][0]['frames']}
        assert diagnostics['activeTransactionScan']['locals'] == 0
        traces.transaction_watchdog(root)
        saved = json.loads((root / 'diagnostics/sqlite-transactions.json').read_text())
        assert len(saved['active']) == 1, saved
        assert not release.is_set()
        print('WATCHDOG_RETURNED_WITH_MUTEX_HELD', flush=True)
    finally:
        release.set()
        thread.join(2)
    assert not thread.is_alive(), 'The fixture owner did not finish'
    assert not errors, errors
    traces.transaction_watchdog(root)
    saved = json.loads((root / 'diagnostics/sqlite-transactions.json').read_text())
    assert saved['active'] == []
    assert saved['recent'][-1]['state'] == 'rolledBack'
    print('OWNER_COMPLETED', flush=True)


class SQLiteTraceThreadSafetyContract(unittest.TestCase):
    def setUp(self):
        importlib.reload(traces)
        temporary = tempfile.TemporaryDirectory(prefix='sqlite-trace-thread-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def test_real_connection_mutex_cannot_block_the_watchdog(self):
        try:
            result = subprocess.run([sys.executable, '-B', str(Path(__file__).resolve()),
                                     '--mutex-child', str(self.root)],
                                    capture_output=True, text=True, timeout=5)
        except subprocess.TimeoutExpired as error:
            self.fail('The diagnostic child deadlocked; the parent watchdog terminated it: ' + str(error.stdout))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            'MUTEX_HELD', 'WATCHDOG_RETURNED_WITH_MUTEX_HELD', 'OWNER_COMPLETED'])

    def test_registry_snapshots_do_not_dereference_connections(self):
        db = codex_sqlite.connect(':memory:', site='contract.registry')
        try:
            db.execute('BEGIN')
            entry = db._codex_transaction_trace
            def forbidden():
                raise AssertionError('Diagnostics dereferenced a foreign connection')
            with traces._LOCK:
                traces._ACTIVE[entry['id']] = (forbidden, entry)
            with patch.object(traces, 'SLOW_MS', 0):
                active = traces.active_transactions()
                traces.transaction_watchdog(self.root)
            self.assertEqual(active[0]['id'], entry['id'])
            self.assertEqual(active[0]['site'], 'contract.registry')
            self.assertIsInstance(active[0]['durationMs'], (float, int))
            self.assertEqual(active[0]['threadId'], threading.get_ident())
            self.assertIn('frames', active[0])
            self.assertEqual(json.loads((self.root / 'diagnostics/sqlite-transactions.json').read_text())
                             ['active'][0]['id'], entry['id'])
        finally:
            db.rollback()
            db.close()

    def test_weak_owner_callback_removes_only_its_registry_entry(self):
        class OwnerHandle:
            _codex_site = 'contract.collected'
            @property
            def in_transaction(self):
                raise AssertionError('The weak callback inspected SQLite state')
        first, second = OwnerHandle(), OwnerHandle()
        traces.begin_transaction(first, 1000, first._codex_site, 'BEGIN')
        traces.begin_transaction(second, 1000, second._codex_site, 'BEGIN')
        first_id = first._codex_transaction_trace['id']
        second_id = second._codex_transaction_trace['id']
        reference = weakref.ref(first)
        del first
        gc.collect()
        self.assertIsNone(reference())
        with traces._LOCK:
            self.assertNotIn(first_id, traces._ACTIVE)
            self.assertIn(second_id, traces._ACTIVE)
        traces.end_transaction(second, 1, 'ended')

    def test_frame_scan_runs_outside_the_registry_lock(self):
        db = codex_sqlite.connect(':memory:', site='contract.frame_lock')
        try:
            db.execute('BEGIN')
            snapshot = traces.sys._current_frames
            def frames():
                self.assertTrue(traces._LOCK.acquire(blocking=False))
                traces._LOCK.release()
                return snapshot()
            with patch.object(traces.sys, '_current_frames', frames), patch.object(traces, 'SLOW_MS', 0):
                self.assertEqual(len(traces.active_transactions()), 1)
                traces.transaction_watchdog(self.root)
        finally:
            db.rollback()
            db.close()

    def test_owner_completion_during_scan_is_not_restored(self):
        for read in (traces.active_transactions, lambda: traces.transaction_watchdog(self.root)):
            with self.subTest(read=read):
                db = codex_sqlite.connect(':memory:', site='contract.completed_race')
                try:
                    db.execute('BEGIN')
                    snapshot = traces.sys._current_frames
                    def frames():
                        result = snapshot()
                        db.commit()
                        return result
                    with patch.object(traces.sys, '_current_frames', frames), patch.object(traces, 'SLOW_MS', 0):
                        result = read()
                    if result is not None:
                        self.assertEqual(result, [])
                    self.assertEqual(traces._ACTIVE, {})
                    self.assertNotIn('active', traces.history())
                    self.assertEqual(traces.active_transactions(), [])
                finally:
                    db.close()


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--mutex-child':
        mutex_case(Path(sys.argv[2]))
    else:
        unittest.main()
