#!/usr/bin/env python3
"""The server index attempt preserves rows and cannot repeat after failure."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_turn_scope_index_update as update

spec = importlib.util.spec_from_file_location('index_update_fixture',
    Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
REVIEWED_RUNTIME = subprocess.check_output(['git', 'show',
    '85f3f8d892727837f79c11f3e6c41dfc18694d1d:scripts/codex_runtime.py'], cwd=ROOT)


@contextmanager
def reviewed_runtime():
    with tempfile.TemporaryDirectory(prefix='studio-index-reviewed-source-') as folder:
        source = Path(folder) / 'codex_runtime.py'
        source.write_bytes(REVIEWED_RUNTIME)
        helper = source.with_name('codex_turn_scope_index_update.py')
        helper.write_bytes(Path(update.__file__).read_bytes())
        module = ModuleType('codex_runtime')
        module.__file__ = str(source)
        with (patch.dict(sys.modules, {'codex_runtime': module}),
              patch.dict(globals(), {'codex_runtime': module}),
              patch.object(update, '__file__', str(helper))):
            exec(compile(REVIEWED_RUNTIME, str(source), 'exec'), vars(module))
            module.Runtime.image_workspace_support = staticmethod(fixture.Runtime.image_workspace_support)
            with patch.object(fixture, 'Runtime', module.Runtime):
                yield module


class TurnScopeIndexUpdate(unittest.TestCase):
    def setUp(self):
        self.enterContext(reviewed_runtime())
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.FakeServer)
        self.runtime.closed = True
        self.runtime.changed.set()
        self.runtime.scheduler.join(timeout=5)
        self.assertFalse(self.runtime.scheduler.is_alive())
        self.runtime.closed = False
        with self.runtime.lock, self.runtime.db() as db:
            db.execute('DROP INDEX IF EXISTS runtime_item_turn_scope')
            db.executemany('INSERT INTO runtime_items VALUES (?,?,?,?)', (
                ('item-' + str(i), 'actor-' + str(i % 2), json.dumps({
                    'id': 'item-' + str(i), 'text': 'Exact text ' + 'x' * 512,
                    **({'turnId': None} if i % 3 == 0 else {'turnId': 'turn-' + str(i % 5)}),
                }), i + .25) for i in range(2000)))

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def rows(self):
        with self.runtime.db() as db:
            return db.execute('SELECT id,agent,record,created FROM runtime_items ORDER BY id').fetchall()

    def index_sql(self):
        with self.runtime.db() as db:
            row = db.execute('SELECT sql FROM sqlite_master WHERE name=?', (update.INDEX,)).fetchone()
            return row[0] if row else None

    def failure_details(self, error):
        return json.loads(str(error).split(': ', 1)[1])

    @contextmanager
    def trace(self):
        queries = []
        connect = codex_runtime.sqlite_connect

        def traced(*args, **kwargs):
            db = connect(*args, **kwargs)
            db.set_trace_callback(queries.append)
            return db
        with patch.object(codex_runtime, 'sqlite_connect', traced):
            yield queries

    def test_create_and_repeat_preserve_every_item_row(self):
        before = self.rows()
        with self.trace() as queries:
            self.assertEqual(update.apply(self.runtime), {'status': 'applied'})
            self.assertEqual(update.apply(self.runtime), {'status': 'already_applied'})
        self.assertEqual(sum(query == update.SQL for query in queries), 1)
        self.assertEqual(self.rows(), before)
        self.assertEqual(vars(self.runtime)[update.ATTEMPT_KEY]['status'], 'applied')
        with self.runtime.db() as db:
            plan = ' '.join(row[3] for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT id FROM runtime_items WHERE agent=? "
                "AND json_extract(record,'$.turnId')=? AND created>=?", ('actor-1', 'turn-1', 0)))
        self.assertIn('runtime_item_turn_scope', plan)
        self.assertIn('<expr>=?', plan)

    def test_existing_exact_index_confirms_without_a_new_attempt(self):
        with self.runtime.db() as db:
            db.execute(update.SQL)
        before = self.rows()
        with self.trace() as queries:
            self.assertEqual(update.apply(self.runtime), {'status': 'already_applied'})
        self.assertNotIn(update.SQL, queries)
        self.assertNotIn(update.ATTEMPT_KEY, vars(self.runtime))
        self.assertEqual(self.rows(), before)

    def test_wrong_index_or_json_path_never_confirms_application(self):
        before = self.rows()
        for expression in ('agent,created', "agent,json_extract(record,'$.turnid'),created",
                           "agent,json_extract(record,'$.turn Id'),created"):
            with self.subTest(expression=expression):
                wrong = 'CREATE INDEX runtime_item_turn_scope ON runtime_items(' + expression + ')'
                with self.runtime.db() as db:
                    db.execute(wrong)
                saved = self.index_sql()
                with self.trace() as queries:
                    with self.assertRaisesRegex(RuntimeError, 'schema differs'):
                        update.apply(self.runtime)
                self.assertNotIn(update.SQL, queries)
                self.assertEqual(self.index_sql(), saved)
                self.assertEqual(self.rows(), before)
                self.assertNotIn(update.ATTEMPT_KEY, vars(self.runtime))
                with self.runtime.db() as db:
                    db.execute('DROP INDEX runtime_item_turn_scope')

    def test_deadline_interrupt_rolls_back_and_blocks_a_second_create(self):
        before = self.rows()
        with self.trace() as queries:
            def clock():
                return 13 if update.SQL in queries else 0
            with patch.object(update, '_clock', clock):
                with self.assertRaisesRegex(RuntimeError, 'pending manual review') as failed:
                    update.apply(self.runtime)
            self.assertEqual(failed.exception.__cause__.sqlite_errorname, 'SQLITE_INTERRUPT')
            self.assertIsNone(self.index_sql())
            self.assertEqual(self.rows(), before)
            saved = dict(vars(self.runtime)[update.ATTEMPT_KEY])
            self.assertEqual(saved['status'], 'failed')
            self.assertEqual(saved['error'], 'OperationalError')
            self.assertEqual(saved['sqlite_errorname'], 'SQLITE_INTERRUPT')
            self.assertEqual(saved['sqlite_errorcode'], 9)
            self.assertEqual(saved['message'], 'interrupted')
            self.assertEqual(saved['elapsedSeconds'], 13)
            self.assertGreaterEqual(saved['failedAt'], saved['startedAt'])
            first_details = self.failure_details(failed.exception)
            self.assertEqual(first_details, {key: value for key, value in saved.items()
                                           if key != 'database'})
            # LiveUpdates creates a new module for each retry.
            retry = ModuleType('turn_scope_index_retry')
            retry.__file__ = update.__file__
            exec(compile(Path(update.__file__).read_bytes(), update.__file__, 'exec'), vars(retry))
            with self.assertRaisesRegex(RuntimeError, 'pending manual review') as pending:
                retry.apply(self.runtime)
            self.assertEqual(self.failure_details(pending.exception), first_details)
            self.assertEqual(vars(self.runtime)[update.ATTEMPT_KEY], saved)
        self.assertEqual(sum(query == update.SQL for query in queries), 1)
        self.assertEqual(sum(query == 'BEGIN IMMEDIATE' for query in queries), 1)
        self.assertIsNone(self.index_sql())

    def test_legacy_failure_details_survive_reload_without_another_attempt(self):
        saved = {'status': 'failed', 'error': 'OperationalError',
                 'startedAt': 100, 'failedAt': 112.5}
        self.runtime.__dict__[update.ATTEMPT_KEY] = dict(saved)
        retry = ModuleType('turn_scope_index_legacy_retry')
        retry.__file__ = update.__file__
        exec(compile(Path(update.__file__).read_bytes(), update.__file__, 'exec'), vars(retry))
        with self.trace() as queries:
            with self.assertRaisesRegex(RuntimeError, 'pending manual review') as pending:
                retry.apply(self.runtime)
        self.assertEqual(self.failure_details(pending.exception),
                         {**saved, 'elapsedSeconds': 12.5})
        self.assertEqual(vars(self.runtime)[update.ATTEMPT_KEY], saved)
        self.assertNotIn('BEGIN IMMEDIATE', queries)
        self.assertNotIn(update.SQL, queries)

    def test_failed_attempt_can_confirm_an_exact_manual_index(self):
        self.runtime.__dict__[update.ATTEMPT_KEY] = {'status': 'failed'}
        with self.runtime.db() as db:
            db.execute(update.SQL)
        with self.trace() as queries:
            self.assertEqual(update.apply(self.runtime), {'status': 'already_applied'})
        self.assertNotIn(update.SQL, queries)

    def test_dropped_index_does_not_repeat_a_successful_heavy_attempt(self):
        self.assertEqual(update.apply(self.runtime), {'status': 'applied'})
        with self.runtime.db() as db:
            db.execute('DROP INDEX runtime_item_turn_scope')
        with self.trace() as queries:
            with self.assertRaisesRegex(RuntimeError, 'pending manual review'):
                update.apply(self.runtime)
        self.assertNotIn(update.SQL, queries)

    def test_changed_database_callback_rejects_before_sql(self):
        with self.trace() as queries, patch.object(self.runtime, 'db', lambda **kwargs: None):
            with self.assertRaisesRegex(RuntimeError, 'database guard differs'):
                update.apply(self.runtime)
        self.assertEqual(queries, [])
        self.assertNotIn(update.ATTEMPT_KEY, vars(self.runtime))

    def test_changed_database_closure_cannot_hide_behind_the_wrapped_fingerprint(self):
        called = []
        def other_database(runtime, **options):
            called.append(options)
            yield None
        original = codex_runtime.Runtime.db
        cell = (lambda value: lambda: value)(other_database).__closure__
        replacement = FunctionType(original.__code__, original.__globals__,
                                   original.__name__, original.__defaults__, cell)
        replacement.__wrapped__ = original.__wrapped__
        with self.trace() as queries, patch.object(codex_runtime.Runtime, 'db', replacement):
            with self.assertRaisesRegex(RuntimeError, 'database guard differs'):
                update.apply(self.runtime)
        self.assertEqual((called, queries), ([], []))

    def test_changed_state_path_rejects_before_sql(self):
        with self.trace() as queries, patch.object(self.runtime, 'db_path',
                Path(self.temp.name) / 'different.sqlite3'):
            with self.assertRaisesRegex(RuntimeError, 'state identity differs'):
                update.apply(self.runtime)
        self.assertEqual(queries, [])

    def test_changed_lease_file_rejects_before_sql(self):
        path = Path(self.temp.name) / 'runtime.lock'
        retained = path.with_name('retained-runtime.lock')
        path.rename(retained)
        path.touch()
        try:
            with self.trace() as queries:
                with self.assertRaisesRegex(RuntimeError, 'lease identity differs'):
                    update.apply(self.runtime)
            self.assertEqual(queries, [])
        finally:
            path.unlink()
            retained.rename(path)

    def test_changed_source_rejects_before_sql(self):
        with tempfile.TemporaryDirectory() as folder:
            source = Path(folder) / 'codex_runtime.py'
            source.write_bytes(REVIEWED_RUNTIME + b'\n# changed\n')
            with (patch.object(codex_runtime, '__file__', str(source)),
                  patch.object(update, '__file__', str(source.with_name('codex_turn_scope_index_update.py'))),
                  self.trace() as queries):
                with self.assertRaisesRegex(RuntimeError, 'runtime source differs'):
                    update.apply(self.runtime)
            self.assertEqual(queries, [])


if __name__ == '__main__':
    unittest.main()
