#!/usr/bin/env python3
"""Live workspace update guards and the real compact HTTP read path."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from urllib.request import urlopen
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_canvas
import codex_workspace
import codex_workspace_read_update as update
from codex_active_task_update import compile_function, signature
spec = importlib.util.spec_from_file_location('runtime_fixture', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class WorkspaceReadUpdate(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.canvas = codex_canvas.Canvas(Path(self.temp.name))
        self.runtime = self.canvas.runtime = fixture.Runtime(self.canvas.root, fixture.FakeServer)
        self.agent = self.runtime.new_lead({})
        self.server = codex_canvas.make_server(self.canvas)
        self.functions = [codex_workspace.WorkspaceMixin.workspace_snapshot,
                          self.server.RequestHandlerClass.do_GET]
        self.saved = [(f.__code__, f.__defaults__, f.__kwdefaults__) for f in self.functions]
        for name, function in zip(('codex_workspace', 'codex_canvas'), self.functions):
            source = subprocess.check_output(['git', 'show',
                '4d65074:scripts/' + name + '.py'], cwd=ROOT, text=True)
            old = (compile_function(source, 'WorkspaceMixin', 'workspace_snapshot', vars(codex_workspace))
                   if name == 'codex_workspace' else update.handler_function(source, function))
            function.__code__, function.__defaults__, function.__kwdefaults__ = (
                old.__code__, old.__defaults__, old.__kwdefaults__)

    def tearDown(self):
        for function, saved in zip(self.functions, self.saved):
            function.__code__, function.__defaults__, function.__kwdefaults__ = saved
        self.server.server_close()
        self.runtime.close()
        self.temp.cleanup()

    def test_patch_preserves_callbacks_and_serves_only_the_inbox(self):
        callback = self.runtime.workspace_snapshot
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(callback.__func__, self.functions[0])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
        worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        worker.start()
        original = self.runtime.records
        def records(db, table):
            if table == 'checkpoints':
                raise AssertionError('HTTP inbox read checkpoint history')
            return original(db, table)
        try:
            url = f'http://127.0.0.1:{self.server.server_port}'
            with patch.object(self.runtime, 'records', side_effect=records), self.runtime.lock:
                with urlopen(url + '/api/workspace?agent=' + self.agent['id'] + '&view=inbox', timeout=1) as reply:
                    self.assertEqual(json.load(reply), {'inbox': []})
            with urlopen(url + '/api/session') as reply:
                self.assertIn('token', json.load(reply))
            with urlopen(url + '/api/workspace?agent=' + self.agent['id']) as reply:
                self.assertIn('checkpoints', json.load(reply))
        finally:
            self.server.shutdown()
            worker.join(2)

    def test_unknown_http_code_prevents_partial_workspace_update(self):
        before = signature(self.functions[0])
        original = update.EXPECTED
        with patch.object(update, 'EXPECTED', {**original, 'do_GET': ('unknown', original['do_GET'][1])}):
            with self.assertRaisesRegex(RuntimeError, 'Unknown live workspace function'):
                update.apply(self.runtime)
        self.assertEqual(signature(self.functions[0]), before)


if __name__ == '__main__':
    unittest.main()
