#!/usr/bin/env python3
"""Validate the exact live usage update without user logs or native calls."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import types
import unittest
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
from codex_costs import CostReader
import codex_usage_update as update
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec); spec.loader.exec_module(fixture)
root = Path(__file__).resolve().parents[1]

def previous(obj, revision, filename, clsname, names):
    source = subprocess.check_output(['git', 'show', revision + ':scripts/' + filename], cwd=root, text=True)
    module = compile(source, filename, 'exec', dont_inherit=True)
    cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == clsname)
    for name in names:
        old = getattr(obj, name).__func__
        code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)
        function = types.FunctionType(code, old.__globals__.copy(), name, old.__defaults__)
        function.__kwdefaults__ = old.__kwdefaults__
        setattr(obj, name, types.MethodType(function, obj))

class UsageUpdateContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.server = self.runtime.connect()
        self.runtime.limits_lock = threading.Lock()
        self.reader = CostReader(self.temp.name, executable='/does-not-exist', interval=900)
        del self.reader.clock
        previous(self.runtime, '389fcd1fde80f82ce01444f6a9e64068d136184e', 'codex_runtime.py', 'Runtime', ['limits'])
        previous(self.reader, 'e6efaec', 'codex_costs.py', 'CostReader', ['snapshot', '_refresh'])

    def tearDown(self):
        self.reader.close(); self.runtime.close(); self.temp.cleanup()

    def test_update_preserves_connections_state_and_scan(self):
        connections = dict(self.runtime.connection_ids)
        state = self.reader.state
        process = self.reader.process
        calls = list(self.server.calls)
        result = update.apply(self.runtime, self.reader)
        self.assertEqual(result['status'], 'applied')
        self.assertEqual(self.runtime.connection_ids, connections)
        self.assertIs(self.reader.state, state)
        self.assertIs(self.reader.process, process)
        self.assertEqual(self.server.calls, calls)
        self.assertEqual(self.reader.interval, 120)
        self.assertEqual(update.apply(self.runtime, self.reader)['status'], 'already_applied')
        self.assertIsNone(self.runtime.limits()['error'])

    def test_unknown_version_changes_nothing(self):
        self.runtime.limits = types.MethodType(lambda self, **kw: {}, self.runtime)
        before = self.reader.snapshot
        with self.assertRaisesRegex(RuntimeError, 'Unknown usage methods'):
            update.apply(self.runtime, self.reader)
        self.assertEqual(self.reader.snapshot, before)
        self.assertEqual(self.reader.interval, 900)
        self.assertFalse(hasattr(self.reader, 'clock'))

if __name__ == '__main__': unittest.main()
