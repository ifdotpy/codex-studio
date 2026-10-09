"""Cost preparation waits outside SQL snapshots and checks the saved source."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
import os
from pathlib import Path
import sqlite3
import sys
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_session_costs
from codex_source import source_function

spec = importlib.util.spec_from_file_location('cost_snapshot_fixture', SERVER_TESTS_ROOT / 'session-cost-snapshot-contract.py')
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)


class Contract(unittest.TestCase):
    def setUp(self):
        candidate = os.environ.get('STUDIO_COST_PREPARATION_SOURCE')
        if candidate:
            spec = importlib.util.spec_from_file_location('prepared_installed_costs', candidate)
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            replacement = patch.object(codex_session_costs, 'SessionCostReader', module.SessionCostReader)
            replacement.start()
            self.addCleanup(replacement.stop)
        baseline = os.environ.get('STUDIO_COST_PREPARATION_BASELINE')
        if baseline:
            function, _ = source_function(Path(baseline).read_text(), ('SessionCostReader', '_compute'),
                                           vars(codex_session_costs), baseline)
            replacement = patch.object(codex_session_costs.SessionCostReader, '_compute', function)
            replacement.start()
            self.addCleanup(replacement.stop)
        self.case = fixtures.SessionCostSnapshotContract(methodName='runTest')

    @contextmanager
    def compute_worker(self, reader):
        result, errors = [], []
        def compute():
            try:
                result.append(reader._compute_shared('lead', 'lead', refresh=True))
            except BaseException as error:
                errors.append(error)
        worker = threading.Thread(target=compute)
        worker.start()
        yield worker, result, errors

    def check_wait(self, layout, *, filesystem=False):
        with self.case.fixture(layout, claude_only=True) as (reader, writers, history_name, connections, _):
            entered, release = threading.Event(), threading.Event()
            if filesystem:
                original = reader._thread_ids
                def gate(*args):
                    entered.set()
                    if not release.wait(3):
                        raise TimeoutError('The private filesystem gate did not release')
                    return original(*args)
                blocking = patch.object(reader, '_thread_ids', side_effect=gate)
            else:
                guard = threading.Lock()
                reader.accounts = SimpleNamespace(lock=guard, data={'accounts': reader.accounts})
                original = reader._account
                def gate(*args):
                    entered.set()
                    return original(*args)
                guard.acquire()
                blocking = patch.object(reader, '_account', side_effect=gate)
            with blocking, self.compute_worker(reader) as (worker, result, errors):
                try:
                    self.assertTrue(entered.wait(2), 'The actual reader must reach the wait')
                    # The caller has not finished, but a committed analytics
                    # append can checkpoint both WAL files immediately.
                    self.case.append(writers, history_name, layout)
                    for name, writer in writers.items():
                        self.assertEqual(writer.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone(), (0, 0, 0),
                                         name + ' snapshot must end before the external wait')
                    self.assertTrue(worker.is_alive())
                finally:
                    release.set()
                    if not filesystem:
                        guard.release()
                    worker.join(4)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(result[0]['generation'], 2)
            self.assertEqual(result[0]['pricedSamples'], 5 if layout == 'overlay' else 4)
            self.assertFalse(reader.inflight)
            self.case.assert_closed(connections)

    def test_account_lock_wait_releases_single_database_snapshot(self):
        self.check_wait('single')

    def test_account_lock_wait_releases_attached_database_snapshots(self):
        self.check_wait('attached')

    def test_account_lock_wait_releases_legacy_overlay_snapshots(self):
        self.check_wait('overlay')

    def test_claude_filesystem_wait_releases_attached_snapshots(self):
        self.check_wait('attached', filesystem=True)

    def test_same_sequence_correction_rechecks_root_generation(self):
        with self.case.fixture() as (reader, writers, history_name, _, _):
            reader._compute_shared('lead', 'lead', refresh=True)
            original = reader._claude_signature
            changed = []
            def correction(agents):
                result = original(agents)
                if not changed:
                    writer = writers[history_name]
                    writer.execute("UPDATE analytics_usage SET record=json_set(record,'$.delta.inputTokens',5000) WHERE seq=1")
                    writer.execute("UPDATE analytics_usage_roots SET generation=2 WHERE root='lead'")
                    writer.execute("UPDATE analytics_meta SET value='2' WHERE key='usageGeneration'")
                    writer.commit()
                    changed.append(True)
                return result
            with patch.object(reader, '_claude_signature', side_effect=correction):
                result = reader._compute('lead', 'lead')
            self.assertEqual(result['_cacheSource']['usage'], {'maxSeq': 3, 'generation': 2})
            self.assertEqual(result['pricedSamples'], 2)
            self.assertAlmostEqual(result['totalUSD'], .00075)

    def test_changed_agent_root_does_not_publish_under_the_old_team(self):
        with self.case.fixture() as (reader, writers, _, connections, _):
            original = reader._claude_signature
            changed = []
            def move(agents):
                result = original(agents)
                if not changed:
                    writer = writers['canvas']
                    for table in ('analytics_agents', 'runtime_agents'):
                        writer.execute('UPDATE ' + table + " SET record=json_set(record,'$.rootId','other-team') WHERE id='lead'")
                    writer.commit()
                    changed.append(True)
                return result
            with patch.object(reader, '_claude_signature', side_effect=move):
                with self.assertRaisesRegex(RuntimeError, 'chat team changed'):
                    reader._compute_shared('lead', 'lead', refresh=True)
            self.assertFalse(reader.cache)
            self.assertFalse(reader.inflight)
            self.case.assert_closed(connections)

    def test_repeated_source_changes_have_a_bounded_visible_failure(self):
        with self.case.fixture() as (reader, writers, history_name, connections, _):
            calls = []
            original = reader._claude_signature
            def change(agents):
                result = original(agents)
                calls.append(True)
                writer = writers[history_name]
                writer.execute("UPDATE analytics_usage_roots SET generation=generation+1 WHERE root='lead'")
                writer.commit()
                return result
            with patch.object(reader, '_claude_signature', side_effect=change):
                with self.assertRaisesRegex(RuntimeError, 'changed during cost preparation'):
                    reader._compute_shared('lead', 'lead', refresh=True)
            self.assertEqual(len(calls), 2)
            self.assertFalse(reader.cache)
            self.assertFalse(reader.inflight)
            self.case.assert_closed(connections)


if __name__ == '__main__':
    unittest.main()
