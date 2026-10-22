"""Retained desktop helpers import using only their packaged directory and the stdlib."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent


class RetainedHelpers(unittest.TestCase):
    def test_packaged_helpers_need_no_backend_modules(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for source in HERE.glob('*.py'):
                if not source.name.endswith('-test.py'):
                    shutil.copy2(source, root / source.name)
            result = subprocess.run(
                [sys.executable, '-I', '-B', '-c',
                 'import sys; sys.path.insert(0, sys.argv[1]); '
                 'import desktop_process, desktop_updates, install_update; '
                 'assert desktop_updates.PENDING_FILE == "pending-update.json"; '
                 'assert desktop_process.process_launch_command(__import__("os").getpid()); '
                 'updates, vm = install_update.load_updates(sys.argv[1]); '
                 'assert updates.PENDING_FILE == desktop_updates.PENDING_FILE', str(root)],
                capture_output=True, text=True, check=False,
                env={**os.environ, 'CODEX_AGENTS_PYTHON': '/nonexistent'})
            self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main()
