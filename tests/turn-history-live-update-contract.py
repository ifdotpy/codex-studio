#!/usr/bin/env python3
"""A guarded code update preserves exact-turn imports and active reads."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
from functools import partial
import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_connection_recovery
import codex_turn_history_update as update
import codex_turn_recovery
from codex_source import signature, source_function

spec = importlib.util.spec_from_file_location('turn_items_fixture', ROOT / 'tests/read-native-turn-items-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
BASELINE = subprocess.check_output(['git', 'show', '391b3060:scripts/codex_turn_recovery.py'], cwd=ROOT)


def clone(function, namespace):
    result = FunctionType(function.__code__, namespace, function.__name__, function.__defaults__, function.__closure__)
    result.__kwdefaults__ = function.__kwdefaults__
    return result


@contextmanager
def reviewed_fixture():
    source = Path(os.environ.get('STUDIO_TURN_UPDATE_RUNTIME_SOURCE', ROOT / 'scripts/codex_runtime.py')).read_bytes()
    if hashlib.sha256(source).hexdigest() not in update.RUNTIME_HASHES:
        raise AssertionError('The fixture needs a reviewed runtime source; set STUDIO_TURN_UPDATE_RUNTIME_SOURCE')
    with tempfile.TemporaryDirectory(prefix='studio-turn-history-update-') as folder:
        scripts = Path(folder)
        modules = {name: ModuleType(name) for name in
                   ('codex_runtime', 'codex_turn_recovery', 'codex_connection_recovery')}
        module, turns, connection = (modules[name] for name in modules)
        for target, original in ((turns, codex_turn_recovery), (connection, codex_connection_recovery)):
            vars(target).update(vars(original))
        for name, loaded in modules.items():
            loaded.__file__ = str(scripts / (name + '.py'))
            raw = source if name == 'codex_runtime' else (ROOT / 'scripts' / (name + '.py')).read_bytes()
            Path(loaded.__file__).write_bytes(raw)
        owner = type('TurnRecoveryMixin', (), {'__module__': 'codex_turn_recovery'})
        for name in update.TURN_CALLERS:
            setattr(owner, name, clone(getattr(codex_turn_recovery.TurnRecoveryMixin, name), vars(turns)))
        turns.TurnRecoveryMixin = module.TurnRecoveryMixin = owner
        turns.read_native_turn, _ = source_function(BASELINE, ['read_native_turn'], vars(turns), '<baseline-391b3060>')
        method, _ = source_function(source, ['Runtime', 'connection_current'], vars(module))
        module.Runtime = type('Runtime', (owner,), {'__module__': 'codex_runtime', 'connection_current': method})
        for name in update.CONNECTION_CALLERS:
            setattr(connection, name, clone(getattr(codex_connection_recovery, name), vars(connection)))
        runtime = module.Runtime.__new__(module.Runtime)
        runtime.lock = threading.RLock()
        runtime.closed = False
        runtime.servers = {'retained': object()}
        runtime.callbacks = [runtime.reconcile_turn, runtime.apply_turn_recovery]
        runtime.connection_ids = {'retained': 'current'}
        runtime.offline_accounts = set()
        with (patch.dict(sys.modules, modules),
              patch.object(update, '__file__', str(scripts / 'codex_turn_history_update.py'))):
            yield runtime, modules, scripts


class TurnHistoryUpdateContract(unittest.TestCase):
    def assert_rejected(self, runtime, imported, expected='differs'):
        code, defaults, kwdefaults = imported.__code__, imported.__defaults__, imported.__kwdefaults__
        with self.assertRaisesRegex(RuntimeError, expected):
            update.apply(runtime)
        self.assertIs(imported.__code__, code)
        self.assertIs(imported.__defaults__, defaults)
        self.assertIs(imported.__kwdefaults__, kwdefaults)

    def test_old_import_and_bound_callback_read_only_complete_target_items(self):
        with reviewed_fixture() as (runtime, modules, _):
            turns = modules['codex_turn_recovery']
            imported = turns.read_native_turn
            server = fixture.Native()
            callback = partial(imported, server, 'thread', 'target')
            servers, callbacks = runtime.servers, runtime.callbacks
            method_functions = [method.__func__ for method in callbacks]
            self.assertEqual(signature(imported), update.BEFORE)
            self.assertEqual(callback()['items'], server.turns[-1]['items'])
            self.assertEqual(server.returned_foreign_items, 31)
            self.assertEqual(update.apply(runtime), {'status': 'applied'})
            self.assertIs(turns.read_native_turn, imported)
            self.assertIs(callback.func, imported)
            self.assertIs(runtime.servers, servers)
            self.assertIs(runtime.callbacks, callbacks)
            self.assertEqual([method.__func__ for method in callbacks], method_functions)
            server.calls.clear()
            server.returned_foreign_items = 0
            self.assertEqual(callback()['items'], server.turns[-1]['items'])
            self.assertEqual(server.returned_foreign_items, 0)
            self.assertEqual(signature(imported), update.AFTER)
            code = imported.__code__
            self.assertEqual(update.apply(runtime), {'status': 'already_applied'})
            self.assertIs(imported.__code__, code)

    def test_active_old_read_finishes_once_and_later_import_uses_new_code(self):
        with reviewed_fixture() as (runtime, modules, _):
            imported = modules['codex_turn_recovery'].read_native_turn
            entered, release = threading.Event(), threading.Event()
            native = fixture.Native()
            original = native.call
            calls, results, errors = [], [], []
            def call(method, params, timeout=60):
                calls.append(method)
                if len(calls) == 1:
                    entered.set()
                    if not release.wait(3): raise AssertionError('The old read did not finish')
                return original(method, params, timeout)
            native.call = call
            def active():
                try: results.append(imported(native, 'thread', 'target'))
                except Exception as error: errors.append(error)
            thread = threading.Thread(target=active)
            thread.start()
            try:
                self.assertTrue(entered.wait(2))
                self.assertEqual(update.apply(runtime), {'status': 'applied'})
            finally:
                release.set()
                thread.join(3)
            self.assertFalse(thread.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(len(results), 1)
            self.assertEqual(results[0]['items'], native.turns[-1]['items'])
            self.assertEqual(native.returned_foreign_items, 31)
            native.returned_foreign_items = 0
            self.assertEqual(imported(native, 'thread', 'target')['items'], native.turns[-1]['items'])
            self.assertEqual(native.returned_foreign_items, 0)

    def test_all_source_changes_reject_before_code_mutation(self):
        for name in ('codex_runtime', 'codex_turn_recovery', 'codex_connection_recovery'):
            with self.subTest(source=name), reviewed_fixture() as (runtime, modules, scripts):
                imported = modules['codex_turn_recovery'].read_native_turn
                path = scripts / (name + '.py')
                path.write_bytes(path.read_bytes() + b'\n# foreign change\n')
                self.assert_rejected(runtime, imported, 'reviewed turn history source differs')

    def test_changed_caller_functions_reject_before_code_mutation(self):
        for module_name, owner, names in (
                ('codex_turn_recovery', 'TurnRecoveryMixin', update.TURN_CALLERS),
                ('codex_runtime', 'Runtime', {'connection_current': update.CONNECTION_CURRENT}),
                ('codex_connection_recovery', None, update.CONNECTION_CALLERS)):
            for name in names:
                with self.subTest(caller=name), reviewed_fixture() as (runtime, modules, _):
                    imported = modules['codex_turn_recovery'].read_native_turn
                    target = getattr(modules[module_name], owner) if owner else modules[module_name]
                    setattr(target, name, lambda *_args: None)
                    self.assert_rejected(runtime, imported)

    def test_foreign_caller_globals_reject_even_with_the_same_code(self):
        for module_name, owner, names in (
                ('codex_turn_recovery', 'TurnRecoveryMixin', update.TURN_CALLERS),
                ('codex_runtime', 'Runtime', {'connection_current': update.CONNECTION_CURRENT}),
                ('codex_connection_recovery', None, update.CONNECTION_CALLERS)):
            for name in names:
                with self.subTest(caller=name), reviewed_fixture() as (runtime, modules, _):
                    imported = modules['codex_turn_recovery'].read_native_turn
                    target = getattr(modules[module_name], owner) if owner else modules[module_name]
                    method = getattr(target, name)
                    foreign = clone(method, dict(method.__globals__))
                    self.assertEqual(signature(foreign), signature(method))
                    setattr(target, name, foreign)
                    self.assert_rejected(runtime, imported)

    def test_foreign_helper_globals_reject_before_any_import_changes(self):
        with reviewed_fixture() as (runtime, modules, _):
            imported = modules['codex_turn_recovery'].read_native_turn
            foreign = clone(imported, dict(imported.__globals__))
            modules['codex_turn_recovery'].read_native_turn = foreign
            self.assert_rejected(runtime, imported, 'callback globals differ')
            self.assertEqual(signature(foreign), update.BEFORE)

    def test_changed_owner_class_or_runtime_bound_caller_rejects(self):
        for change in ('owner', 'class', 'instance', 'guard'):
            with self.subTest(change=change), reviewed_fixture() as (runtime, modules, _):
                imported = modules['codex_turn_recovery'].read_native_turn
                if change == 'owner':
                    modules['codex_runtime'].TurnRecoveryMixin = type('ForeignOwner', (), {})
                elif change == 'class':
                    current = modules['codex_runtime'].Runtime.reconcile_turn
                    modules['codex_runtime'].Runtime.reconcile_turn = clone(current, current.__globals__)
                elif change == 'instance':
                    runtime.reconcile_turn = runtime.reconcile_turn
                else:
                    runtime.connection_current = runtime.connection_current
                self.assert_rejected(runtime, imported)

    def test_changed_running_helper_or_extracted_function_rejects(self):
        with reviewed_fixture() as (runtime, modules, _):
            imported = modules['codex_turn_recovery'].read_native_turn
            modules['codex_turn_recovery'].read_native_turn = clone(lambda *_args: None, vars(modules['codex_turn_recovery']))
            self.assert_rejected(runtime, imported)
        with reviewed_fixture() as (runtime, modules, _):
            imported = modules['codex_turn_recovery'].read_native_turn
            with patch.object(update, 'source_function', return_value=(imported, False)):
                self.assert_rejected(runtime, imported, 'reviewed turn history function differs')

    def test_wrong_module_path_or_runtime_owner_rejects(self):
        for change in ('path', 'instance'):
            with self.subTest(change=change), reviewed_fixture() as (runtime, modules, scripts):
                imported = modules['codex_turn_recovery'].read_native_turn
                if change == 'path': modules['codex_connection_recovery'].__file__ = str(scripts.parent / 'foreign.py')
                else: runtime = object()
                self.assert_rejected(runtime, imported, 'source identity differs')

    def test_stop_source_and_module_changes_at_lock_boundary_reject(self):
        for change in ('stop', 'source', 'module'):
            with self.subTest(change=change), reviewed_fixture() as (runtime, modules, scripts):
                imported = modules['codex_turn_recovery'].read_native_turn
                lock = runtime.lock
                class Boundary:
                    def __enter__(self):
                        lock.acquire()
                        if change == 'stop': runtime.closed = True
                        elif change == 'source':
                            path = scripts / 'codex_turn_recovery.py'
                            path.write_bytes(path.read_bytes() + b'\n# changed before mutation\n')
                        else: sys.modules['codex_turn_recovery'] = ModuleType('foreign')
                    def __exit__(self, *_args): lock.release()
                runtime.lock = Boundary()
                self.assert_rejected(runtime, imported, 'closed|changed before the update')

    def test_already_applied_still_checks_caller_guards(self):
        with reviewed_fixture() as (runtime, modules, _):
            self.assertEqual(update.apply(runtime), {'status': 'applied'})
            imported = modules['codex_turn_recovery'].read_native_turn
            runtime.connection_current = runtime.connection_current
            self.assert_rejected(runtime, imported, 'guard differs')


if __name__ == '__main__':
    unittest.main()
