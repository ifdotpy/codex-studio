#!/usr/bin/env python3
"""Guard the one-function cost update and preserve active and bound callers."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_cost_invalidation_update as update
from codex_source import signature, source_function

spec = importlib.util.spec_from_file_location('invalidation_fixture', ROOT / 'tests/session-cost-invalidation-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
BASELINE = subprocess.check_output(['git', 'show', '31f92edd:scripts/codex_analytics.py'], cwd=ROOT)
INTERMEDIATE = subprocess.check_output(['git', 'show', '85f3f8d892727837f79c11f3e6c41dfc18694d1d:scripts/codex_analytics.py'], cwd=ROOT)
REVIEWED_RUNTIME = subprocess.check_output(['git', 'show', '85f3f8d892727837f79c11f3e6c41dfc18694d1d:scripts/codex_runtime.py'], cwd=ROOT)


@contextmanager
def reviewed_fixture(baseline=BASELINE):
    with tempfile.TemporaryDirectory(prefix='studio-cost-update-') as folder:
        scripts = Path(folder)
        path = scripts / 'codex_analytics.py'
        raw = (ROOT / 'scripts/codex_analytics.py').read_bytes()
        path.write_bytes(raw)
        analytics = ModuleType('codex_analytics')
        analytics.__file__ = str(path)
        exec(compile(raw, str(path), 'exec'), vars(analytics))
        old, _ = source_function(baseline, ['AnalyticsMixin', 'analytics_event'], vars(analytics), '<baseline>')
        analytics.AnalyticsMixin.analytics_event = old
        runtime_module = ModuleType('codex_runtime')
        runtime_path = scripts / 'codex_runtime.py'
        runtime_path.write_bytes(REVIEWED_RUNTIME)
        runtime_module.__file__ = str(runtime_path)
        runtime_module.Runtime = type('Runtime', (analytics.AnalyticsMixin,), {
            'records': lambda _self, _db, _table: [],
        })
        runtime = runtime_module.Runtime()
        runtime.lock = threading.RLock()
        runtime.closed = False
        runtime.servers = {'fixture': object()}
        runtime.cache = {'retained': object()}
        with (patch.dict(sys.modules, {'codex_runtime': runtime_module, 'codex_analytics': analytics}),
              patch.object(update, '__file__', str(scripts / 'codex_cost_invalidation_update.py'))):
            yield runtime, analytics, path, runtime_path


@contextmanager
def cost_fixture(runtime):
    case = fixture.SessionCostInvalidationContract()
    case.setUp()
    case.capture = runtime
    try:
        yield case
    finally:
        case.doCleanups()


class CostInvalidationUpdateContract(unittest.TestCase):
    def test_applied_intermediate_callback_upgrades_and_preserves_claude_receipt(self):
        with reviewed_fixture(INTERMEDIATE) as (runtime, analytics, _, _), cost_fixture(runtime) as case:
            callback = runtime.analytics_event
            function = callback.__func__
            self.assertEqual(signature(function), update.INTERMEDIATE)
            servers, cache = runtime.servers, runtime.cache
            notice = case.claude_notice()
            self.assertEqual(case.event(notice), 160)
            observed = case.notice(responseId='response')
            observed['tokenUsage']['modelContextWindow'] = 200000
            observed_spent = case.event(observed, at=100.5)
            first = case.estimate()
            original, initial = case.usage(), case.state()
            self.assertEqual(update.apply(runtime), {'status': 'applied'})
            self.assertIs(analytics.AnalyticsMixin.analytics_event, function)
            self.assertIs(callback.__func__, function)
            self.assertIs(runtime.servers, servers)
            self.assertIs(runtime.cache, cache)
            with case.db:
                self.assertEqual(callback(case.db, case.agent, 'thread/tokenUsage/updated', notice, at=101), observed_spent)
            second = case.estimate()
            self.assertEqual(case.usage(), original)
            self.assertEqual(case.state(), initial)
            self.assertEqual(case.projections, ['lead'])
            self.assertEqual(second['totalUSD'], first['totalUSD'])
            self.assertEqual(case.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 2)
            self.assertEqual(case.db.execute('SELECT sum(count) FROM analytics_notifications').fetchone()[0], 3)
            code = function.__code__
            self.assertEqual(update.apply(runtime), {'status': 'already_applied'})
            self.assertIs(function.__code__, code)

    def test_active_intermediate_claude_frame_finishes_before_later_duplicate_skip(self):
        with reviewed_fixture(INTERMEDIATE) as (runtime, _, _, _), cost_fixture(runtime) as case:
            callback = runtime.analytics_event
            self.assertEqual(signature(callback.__func__), update.INTERMEDIATE)
            notice = case.claude_notice()
            case.event(notice)
            case.estimate()
            entered, release = threading.Event(), threading.Event()
            errors = []

            class GateDB:
                def __init__(self, connection):
                    self.connection = connection

                def execute(self, sql, *args, **kwargs):
                    if "SELECT value FROM analytics_meta WHERE key='usageGeneration'" in sql:
                        entered.set()
                        if not release.wait(3):
                            raise AssertionError('The intermediate frame was not released')
                    return self.connection.execute(sql, *args, **kwargs)

                def __getattr__(self, name):
                    return getattr(self.connection, name)

            def active():
                db = sqlite3.connect(case.path)
                db.row_factory = sqlite3.Row
                try:
                    with db:
                        callback(GateDB(db), case.agent, 'thread/tokenUsage/updated', notice, at=101)
                except Exception as error:
                    errors.append(error)
                finally:
                    db.close()

            worker = threading.Thread(target=active)
            worker.start()
            try:
                self.assertTrue(entered.wait(2))
                self.assertEqual(update.apply(runtime), {'status': 'applied'})
            finally:
                release.set()
                worker.join(3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(case.state(), {'maxSeq': 1, 'generation': 2})
            self.assertEqual(case.db.execute('SELECT at FROM analytics_usage').fetchone()[0], 101)
            original = case.usage()
            case.estimate()
            case.event(notice, at=102)
            case.estimate()
            self.assertEqual(case.usage(), original)
            self.assertEqual(case.projections, ['lead', 'lead'])
            self.assertEqual(case.state()['generation'], 2)
            self.assertEqual(case.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)

    def test_bound_callback_uses_the_new_code_and_keeps_exact_budget_and_cache(self):
        with reviewed_fixture() as (runtime, analytics, _, _), cost_fixture(runtime) as case:
            callback = runtime.analytics_event
            function = callback.__func__
            servers, cache = runtime.servers, runtime.cache
            self.assertEqual(signature(function), update.BEFORE)
            case.event()
            first = case.estimate()
            self.assertEqual(update.apply(runtime), {'status': 'applied'})
            self.assertIs(analytics.AnalyticsMixin.analytics_event, function)
            self.assertIs(callback.__func__, function)
            self.assertIs(runtime.servers, servers)
            self.assertIs(runtime.cache, cache)
            with case.db:
                self.assertEqual(callback(case.db, case.agent, 'thread/tokenUsage/updated',
                                          case.notice(), at=101), 1100)
            second = case.estimate()
            self.assertEqual(case.projections, ['lead'])
            self.assertEqual(second['totalUSD'], first['totalUSD'])
            self.assertEqual(case.state(), {'maxSeq': 1, 'generation': 1})
            self.assertEqual(case.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)
            code = function.__code__
            self.assertEqual(update.apply(runtime), {'status': 'already_applied'})
            self.assertIs(function.__code__, code)

    def test_an_active_old_frame_finishes_once_and_later_bound_calls_use_new_code(self):
        with reviewed_fixture() as (runtime, _, _, _), cost_fixture(runtime) as case:
            callback = runtime.analytics_event
            case.event()
            case.estimate()
            entered, release = threading.Event(), threading.Event()
            errors = []
            class GateDB:
                def __init__(self, connection):
                    self.connection = connection
                def execute(self, sql, *args, **kwargs):
                    if "SELECT value FROM analytics_meta WHERE key='usageGeneration'" in sql:
                        entered.set()
                        if not release.wait(3):
                            raise AssertionError('The old analytics frame was not released')
                    return self.connection.execute(sql, *args, **kwargs)
                def __getattr__(self, name):
                    return getattr(self.connection, name)
            def active():
                db = sqlite3.connect(case.path)
                db.row_factory = sqlite3.Row
                try:
                    with db:
                        callback(GateDB(db), case.agent, 'thread/tokenUsage/updated', case.notice(), at=101)
                except Exception as error:
                    errors.append(error)
                finally:
                    db.close()
            worker = threading.Thread(target=active)
            worker.start()
            try:
                self.assertTrue(entered.wait(2))
                self.assertEqual(update.apply(runtime), {'status': 'applied'})
            finally:
                release.set()
                worker.join(3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(case.state(), {'maxSeq': 1, 'generation': 2})
            case.estimate()
            case.event(at=102)
            case.estimate()
            self.assertEqual(case.projections, ['lead', 'lead'])
            self.assertEqual(case.state()['generation'], 2)
            self.assertEqual(case.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)

    def test_changed_analytics_source_rejects_before_callback_mutation(self):
        with reviewed_fixture() as (runtime, _, path, _):
            callback = runtime.analytics_event
            code = callback.__func__.__code__
            path.write_bytes(path.read_bytes() + b'\n# foreign change\n')
            with self.assertRaisesRegex(RuntimeError, 'reviewed analytics source differs'):
                update.apply(runtime)
            self.assertIs(callback.__func__.__code__, code)

    def test_changed_runtime_source_rejects_before_callback_mutation(self):
        with reviewed_fixture() as (runtime, _, _, path):
            code = runtime.analytics_event.__func__.__code__
            path.write_bytes(path.read_bytes() + b'\n# foreign change\n')
            with self.assertRaisesRegex(RuntimeError, 'backend source differs'):
                update.apply(runtime)
            self.assertIs(runtime.analytics_event.__func__.__code__, code)

    def test_changed_dependencies_reject_before_callback_mutation(self):
        for name in update.DEPENDENCIES:
            with self.subTest(name=name), reviewed_fixture() as (runtime, analytics, _, _):
                code = runtime.analytics_event.__func__.__code__
                setattr(analytics.AnalyticsMixin, name, lambda *_args, **_kwargs: None)
                with self.assertRaisesRegex(RuntimeError, 'analytics guard differs: ' + name):
                    update.apply(runtime)
                self.assertIs(runtime.analytics_event.__func__.__code__, code)

    def test_instance_dependency_override_rejects_before_callback_mutation(self):
        with reviewed_fixture() as (runtime, _, _, _):
            code = runtime.analytics_event.__func__.__code__
            runtime.analytics_budget_capture = lambda *_args, **_kwargs: 0
            with self.assertRaisesRegex(RuntimeError, 'analytics guard differs: analytics_budget_capture'):
                update.apply(runtime)
            self.assertIs(runtime.analytics_event.__func__.__code__, code)

    def test_wrong_extracted_function_rejects_before_callback_mutation(self):
        with reviewed_fixture() as (runtime, analytics, _, _):
            code = runtime.analytics_event.__func__.__code__
            baseline, _ = source_function(BASELINE, ['AnalyticsMixin', 'analytics_event'], vars(analytics))
            with patch.object(update, 'source_function', return_value=(baseline, False)):
                with self.assertRaisesRegex(RuntimeError, 'reviewed analytics function differs'):
                    update.apply(runtime)
            self.assertIs(runtime.analytics_event.__func__.__code__, code)

    def test_unknown_current_function_rejects_without_replacement(self):
        with reviewed_fixture() as (runtime, analytics, _, _):
            unknown, _ = source_function('def analytics_event(self,*args,**kwargs):\n    return None\n',
                                        ['analytics_event'], vars(analytics))
            analytics.AnalyticsMixin.analytics_event.__code__ = unknown.__code__
            code = runtime.analytics_event.__func__.__code__
            with self.assertRaisesRegex(RuntimeError, 'running analytics function differs'):
                update.apply(runtime)
            self.assertIs(runtime.analytics_event.__func__.__code__, code)

    def test_matching_code_in_foreign_globals_cannot_replace_the_callback(self):
        with reviewed_fixture() as (runtime, analytics, _, _):
            old = analytics.AnalyticsMixin.analytics_event
            foreign = FunctionType(old.__code__, dict(vars(analytics)), old.__name__, old.__defaults__)
            foreign.__kwdefaults__ = old.__kwdefaults__
            self.assertEqual(signature(foreign), update.BEFORE)
            analytics.AnalyticsMixin.analytics_event = foreign
            with self.assertRaisesRegex(RuntimeError, 'callback identity differs'):
                update.apply(runtime)
            self.assertIs(runtime.analytics_event.__func__, foreign)

    def test_close_while_the_update_waits_for_runtime_lock_preserves_old_code(self):
        with reviewed_fixture() as (runtime, _, _, _):
            old = runtime.analytics_event.__func__
            code = old.__code__
            errors = []
            def apply():
                try:
                    update.apply(runtime)
                except Exception as error:
                    errors.append(error)
            with runtime.lock:
                worker = threading.Thread(target=apply)
                worker.start()
                deadline = time.monotonic() + 2
                waiting = False
                while time.monotonic() < deadline:
                    frame = sys._current_frames().get(worker.ident)
                    while frame is not None:
                        if frame.f_code is update.apply.__code__ and 'desired' in frame.f_locals:
                            waiting = True
                            break
                        frame = frame.f_back
                    if waiting:
                        break
                    threading.Event().wait(.005)
                self.assertTrue(waiting, 'The update must wait at the runtime lock')
                self.assertIs(old.__code__, code)
                runtime.closed = True
            worker.join(2)
            self.assertFalse(worker.is_alive())
            self.assertEqual(len(errors), 1)
            self.assertRegex(str(errors[0]), 'backend is closed')
            self.assertIs(old.__code__, code)


if __name__ == '__main__':
    unittest.main()
