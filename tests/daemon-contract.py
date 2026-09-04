"""No processes launched: verify the daemon's environment boundary."""
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
loader = importlib.machinery.SourceFileLoader("daemon", str(SCRIPTS / "codex-daemon"))
spec = importlib.util.spec_from_loader(loader.name, loader)
daemon = importlib.util.module_from_spec(spec)
loader.exec_module(daemon)

class DaemonContract(unittest.TestCase):
    def test_relative_overrides_are_resolved_before_chdir(self):
        previous = Path.cwd()
        with tempfile.TemporaryDirectory(prefix="codex-daemon-contract-") as directory:
            root = Path(directory).resolve()
            try:
                os.chdir(root)
                (root / "tasks.json").write_text("[]")
                overrides = {
                    "CODEX_AGENTS_STATE_DIR": "state",
                    "CODEX_BOARD_STATE_DIR": "board",
                    "CODEX_HOME": "profile",
                    "CODEX_TASKS": "tasks.json",
                }
                child = SimpleNamespace(pid=123, poll=lambda: None)
                with patch.dict(os.environ, overrides), \
                     patch.object(daemon.shutil, "which", return_value="/fake/node"), \
                     patch.object(daemon.subprocess, "Popen", return_value=child) as launch, \
                     patch.object(daemon.time, "sleep"):
                    self.assertEqual(daemon.cmd_start(SimpleNamespace(wave="test.wave")), 0)
                arguments = launch.call_args.kwargs
                self.assertEqual(arguments["cwd"], str(root / "state"))
                for key, relative in overrides.items():
                    self.assertEqual(arguments["env"][key], str(root / relative))
            finally:
                os.chdir(previous)

if __name__ == "__main__":
    unittest.main()
