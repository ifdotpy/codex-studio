#!/usr/bin/env python3
"""Live chat controls preserve the runtime and reject unknown code."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_canvas import Canvas, make_server
from codex_chat_controls_update import apply


class ChatUpdateContract(unittest.TestCase):
    def test_repeated_patch_preserves_objects_and_unknown_code_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = f.Runtime(Path(directory), f.FakeServer)
            canvas = Canvas(Path(directory))
            canvas.runtime = runtime
            server = make_server(canvas)
            try:
                lock = runtime.lock
                send = type(runtime).send
                self.assertEqual(apply(runtime)['status'], 'applied')
                self.assertEqual(apply(runtime)['status'], 'applied')
                self.assertIs(runtime.lock, lock)
                self.assertIs(type(runtime).send, send)
                with patch.object(type(runtime), 'answer', lambda *args: None):
                    with self.assertRaisesRegex(RuntimeError, 'Unknown live chat method'):
                        apply(runtime)
                self.assertIs(type(runtime).send, send)
            finally:
                server.server_close()
                runtime.close()


if __name__ == '__main__':
    unittest.main()
