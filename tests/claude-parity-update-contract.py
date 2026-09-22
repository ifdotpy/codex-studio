#!/usr/bin/env python3
"""Claude live patch rejects unknown inputs and preserves live object identities."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('parity_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_claude
import codex_claude_parity_update as update
from codex_canvas import Canvas, make_server
from codex_progress_update import source_function
from codex_active_task_update import signature


class ParityPatch(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.runtime = f.Runtime(Path(self.temp.name), f.FakeServer)
        self.addCleanup(self.runtime.close)
        canvas = Canvas(Path(self.temp.name))
        canvas.runtime = self.runtime
        self.server = make_server(canvas)
        self.addCleanup(self.server.server_close)
        self.handler = self.server.RequestHandlerClass
        self.saved = []
        self.globals = dict(vars(codex_claude))
        self.addCleanup(self.restore)

    def owner(self, target):
        module_name, *path = target.split('.')
        module = sys.modules[module_name]
        owner = self.handler if path[0] == 'make_server' else getattr(module, path[0]) if len(path) > 1 else module
        return module, path, owner

    def restore(self):
        for owner, name, descriptor, state in reversed(self.saved):
            setattr(owner, name, descriptor)
            live = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            live.__code__, live.__defaults__, live.__kwdefaults__ = state
        for name in set(vars(codex_claude)) - set(self.globals):
            delattr(codex_claude, name)
        vars(codex_claude).update(self.globals)

    def baseline(self):
        sources = {}
        for target, allowed in update.EXPECTED.items():
            module, path, owner = self.owner(target)
            descriptor = vars(owner)[path[-1]]
            live = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            self.saved.append((owner, path[-1], descriptor,
                               (live.__code__, live.__defaults__, live.__kwdefaults__)))
            if allowed[0] is None:
                delattr(owner, path[-1])
                continue
            source = sources.setdefault(module.__name__, subprocess.check_output(
                ['git', 'show', update.BASE_COMMIT + ':scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True))
            old, _ = source_function(source, path, vars(module))
            self.assertEqual(signature(old), allowed[0], target)
            live.__code__, live.__defaults__, live.__kwdefaults__ = old.__code__, old.__defaults__, old.__kwdefaults__
        for name in ('_cache', 're', 'shlex'):
            vars(codex_claude).pop(name, None)

    def test_baseline_repeat_preserves_connections_callbacks_and_http_cells(self):
        self.baseline()
        servers, lock, handler = self.runtime.servers, self.runtime.lock, self.handler
        cells = handler.do_POST.__closure__
        methods = {target: getattr(self.owner(target)[2], self.owner(target)[1][-1])
                   for target, allowed in update.EXPECTED.items() if allowed[0] is not None}
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        cache = codex_claude._cache
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(self.runtime.servers, servers)
        self.assertIs(self.runtime.lock, lock)
        self.assertIs(handler.do_POST.__closure__, cells)
        self.assertIs(codex_claude._cache, cache)
        for target, allowed in update.EXPECTED.items():
            live = getattr(self.owner(target)[2], self.owner(target)[1][-1])
            self.assertEqual(signature(live), allowed[1], target)
            if target in methods:
                self.assertIs(live, methods[target], target)

    def test_unknown_live_code_rejected_before_mutation(self):
        self.baseline()
        with patch.object(type(self.runtime), 'send', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'Unknown live Claude parity method'):
                update.apply(self.runtime)
        self.assertNotIn('_cache', vars(codex_claude))
        self.assertFalse(hasattr(codex_claude, 'profile_options'))

    def test_source_digest_mismatch_rejected(self):
        name = next(iter(update.FILES))
        with patch.dict(update.FILES, {name: '0' * 64}):
            with self.assertRaisesRegex(RuntimeError, 'Unreviewed Claude parity file'):
                update.apply(self.runtime)

    def test_install_failure_restores_methods_and_globals(self):
        self.baseline()
        expected = {target: signature(getattr(self.owner(target)[2], self.owner(target)[1][-1]))
                    for target, allowed in update.EXPECTED.items() if allowed[0] is not None}
        real = update._install
        calls = 0
        def fail(*args):
            nonlocal calls
            real(*args)
            calls += 1
            if calls == len(update.EXPECTED) - 1:
                raise RuntimeError('Fixture install failure')
        with patch.object(update, '_install', side_effect=fail):
            with self.assertRaisesRegex(RuntimeError, 'Fixture install failure'):
                update.apply(self.runtime)
        self.assertNotIn('_cache', vars(codex_claude))
        self.assertFalse(hasattr(codex_claude, 'profile_options'))
        for target, digest in expected.items():
            self.assertEqual(signature(getattr(self.owner(target)[2], self.owner(target)[1][-1])), digest)


if __name__ == '__main__':
    unittest.main()
