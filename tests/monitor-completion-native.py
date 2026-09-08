#!/usr/bin/env python3
"""Real local command exit and cancellation through Studio, without model calls."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_runtime import AppServer, Runtime


class NativeCommands(f.FakeServer):
    def __init__(self, root, notify, request, died):
        super().__init__(root, notify, request, died)
        home = root / 'native-home'
        home.mkdir()
        (home / 'config.toml').write_text('[features]\nplugins=false\nremote_plugin=false\napps=false\nskip_host_skill_discovery=true\n')
        self.native = AppServer(root, notify, request, died, home=home, isolated=True)

    def submit(self, method, params):
        if method == 'command/exec':
            self.calls.append((method, params))
            return self.native.submit(method, params)
        return super().submit(method, params)

    def wait(self, submitted, timeout=60):
        return self.native.wait(submitted, timeout) if isinstance(submitted, tuple) else super().wait(submitted, timeout)

    def call(self, method, params, timeout=60):
        if method == 'command/exec/terminate':
            self.calls.append((method, params))
            return self.native.call(method, params, timeout)
        return super().call(method, params, timeout)

    def after_events(self, callback):
        self.native.after_events(callback)

    def close(self):
        self.native.close()
        super().close()


class NativeCompletion(unittest.TestCase):
    def test_real_exit_and_cancel_emit_one_terminal_receipt(self):
        with tempfile.TemporaryDirectory(prefix='studio-monitor-completion-') as directory:
            root = Path(directory)
            runtime = Runtime(root, NativeCommands)
            try:
                lead = runtime.create({'name': 'Native command fixture', 'prompt': 'Fixture', 'cwd': directory, 'model': 'gpt-5.6-sol'})
                f.eventually(lambda: runtime.agent(lead['id'])['status'] == 'running')
                def record(key):
                    with runtime.db() as db:
                        return json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?', (key,)).fetchone()[0])
                def events(key):
                    with runtime.db() as db:
                        return db.execute('SELECT text FROM runtime_events WHERE id=?', ('monitor:' + key,)).fetchall()
                success = runtime.monitor(lead['id'], {'command': 'printf studio-native-exit', 'timeout_ms': 10000}, approved=True)
                f.eventually(lambda: record(success['id'])['status'] == 'completed', timeout=15)
                self.assertEqual(record(success['id'])['exitCode'], 0)
                self.assertEqual(len(events(success['id'])), 1)
                cancelled = runtime.monitor(lead['id'], {'command': 'printf studio-native-ready; exec sleep 30', 'timeout_ms': 40000}, approved=True)
                f.eventually(lambda: 'studio-native-ready' in record(cancelled['id'])['tail'], timeout=15)
                runtime.cancel_monitor(cancelled['id'])
                f.eventually(lambda: record(cancelled['id'])['status'] == 'cancelled', timeout=15)
                terminal = record(cancelled['id'])
                self.assertIsInstance(terminal['exitCode'], int)
                self.assertNotEqual(terminal['exitCode'], 0)
                self.assertEqual(len(events(cancelled['id'])), 1)
                payload = json.loads(events(cancelled['id'])[0][0])
                self.assertEqual(payload['exitCode'], terminal['exitCode'])
                self.assertEqual(payload['status'], 'cancelled')
                runtime.finish_monitor(cancelled['id'], terminal['exitCode'], None)
                self.assertEqual(len(events(cancelled['id'])), 1)
                print(json.dumps({'successExit': 0, 'cancelExit': terminal['exitCode'], 'receipts': 2}))
            finally:
                runtime.close()


if __name__ == '__main__':
    unittest.main(verbosity=2)
