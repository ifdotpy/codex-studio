#!/usr/bin/env python3
"""Guarded in-process update from the exact previous released source."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
from codex_native_errors_update import apply, BASE, POLICY_GUARDS
from codex_efficiency_update import fingerprint
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

class UpdateContract(unittest.TestCase):
    def test_unknown_policy_guard_rejects_before_any_replacement(self):
        with tempfile.TemporaryDirectory() as temp:
            runtime = Runtime(Path(temp), fixture.FakeServer)
            try:
                runtime.start = types.MethodType(lambda self, *args: None, runtime)
                before = {name: getattr(runtime, name) for name in (*BASE, *POLICY_GUARDS)}
                with self.assertRaisesRegex(RuntimeError, 'Native policy guard start requires current source'):
                    apply(runtime)
                self.assertEqual(before, {name: getattr(runtime, name) for name in before})
            finally:
                runtime.close()

    def test_historical_submission_guards_reject_a_partial_policy_update(self):
        root = Path(__file__).resolve().parents[1]
        source = subprocess.check_output(['git', 'show', 'a2a2cce:scripts/codex_runtime.py'], cwd=root, text=True)
        module = compile(source, 'previous-runtime', 'exec', dont_inherit=True)
        cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
        for name in POLICY_GUARDS:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temp:
                runtime = Runtime(Path(temp), fixture.FakeServer)
                try:
                    previous = getattr(runtime, name).__func__
                    code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)
                    fn = types.FunctionType(code, previous.__globals__, name, previous.__defaults__)
                    fn.__kwdefaults__ = previous.__kwdefaults__
                    setattr(runtime, name, types.MethodType(fn, runtime))
                    before = {key: getattr(runtime, key) for key in (*BASE, *POLICY_GUARDS)}
                    with self.assertRaisesRegex(RuntimeError, 'requires current source'):
                        apply(runtime)
                    self.assertEqual(before, {key: getattr(runtime, key) for key in before})
                finally:
                    runtime.close()

    def test_exact_previous_release_updates_without_replacing_processes_or_pools(self):
        root = Path(__file__).resolve().parents[1]
        source = subprocess.check_output(['git', 'show', 'a2a2cce:scripts/codex_runtime.py'], cwd=root, text=True)
        module = compile(source, 'previous-runtime', 'exec', dont_inherit=True)
        cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
        with tempfile.TemporaryDirectory() as temp:
            runtime = Runtime(Path(temp), fixture.FakeServer)
            try:
                server = runtime.connect()
                a = runtime.create({'name': 'Lead', 'cwd': temp, 'prompt': 'Work'})
                fixture.eventually(lambda: bool(runtime.agent(a['id']).get('turnId')))
                with runtime.lock:
                    for name in BASE:
                        previous = getattr(runtime, name).__func__
                        code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)
                        fn = types.FunctionType(code, previous.__globals__, name, previous.__defaults__)
                        fn.__kwdefaults__ = previous.__kwdefaults__
                        setattr(runtime, name, types.MethodType(fn, runtime))
                        self.assertEqual(fingerprint(getattr(runtime, name)), BASE[name])
                    connections = dict(runtime.servers)
                    pools = {name: getattr(runtime, name) for name in ('pool', 'coordination_pool', 'tool_pool', 'recovery_pool')}
                    self.assertEqual(apply(runtime)['status'], 'applied')
                    self.assertEqual(apply(runtime)['status'], 'already_applied')
                    self.assertEqual(runtime.servers, connections)
                    self.assertEqual(pools, {name: getattr(runtime, name) for name in pools})
                a = runtime.agent(a['id'])
                server.notify({'method': 'error', 'params': {'threadId': a['threadId'], 'turnId': a['turnId'],
                               'willRetry': True, 'error': {'message': 'Native retry'}}})
                self.assertEqual(runtime.agent(a['id'])['activity']['phase'], 'retrying')
                self.assertTrue(runtime.agent(a['id'])['inFlight'])
            finally:
                runtime.close()

    def test_old_transport_cannot_receive_a_partial_runtime_update(self):
        with tempfile.TemporaryDirectory() as temp:
            runtime = Runtime(Path(temp), fixture.FakeServer)
            try:
                method = runtime.send.__func__
                scope = method.__globals__.copy()
                scope.pop('SubmissionRejected')
                old = types.FunctionType(method.__code__, scope, 'send', method.__defaults__)
                old.__kwdefaults__ = method.__kwdefaults__
                runtime.send = types.MethodType(old, runtime)
                before = {name: getattr(runtime, name) for name in BASE}
                with self.assertRaisesRegex(RuntimeError, 'transport update required'):
                    apply(runtime)
                self.assertEqual(before, {name: getattr(runtime, name) for name in BASE})
            finally:
                runtime.close()

    def test_unknown_method_rejects_before_any_replacement(self):
        with tempfile.TemporaryDirectory() as temp:
            runtime = Runtime(Path(temp), fixture.FakeServer)
            try:
                runtime.notification = types.MethodType(lambda self, *args: None, runtime)
                before = {name: getattr(runtime, name) for name in BASE}
                with self.assertRaisesRegex(RuntimeError, 'Unknown runtime method notification'):
                    apply(runtime)
                self.assertEqual(before, {name: getattr(runtime, name) for name in BASE})
            finally:
                runtime.close()

if __name__ == '__main__': unittest.main()
