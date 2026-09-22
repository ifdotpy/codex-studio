#!/usr/bin/env python3
"""State paths and launcher owner fields use the current names."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
from codex_state import read_threads, state_dir


class StatePaths(unittest.TestCase):
    def test_only_the_current_state_variable_overrides_the_default(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            with patch.dict(os.environ, {"CODEX_AGENTS_STATE_DIR": "", "XDG_STATE_HOME": str(root),
                                         "LUNA_HOME": str(root / "old"), "CODEX_BOARD_STATE_DIR": str(root / "board")}):
                self.assertEqual(state_dir(), root / "codex-agents")
                result = subprocess.run([sys.executable, "-B", str(SCRIPTS / "luna"), "ls"],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse((root / "old").exists())
                with patch.dict(os.environ, {"CODEX_AGENTS_STATE_DIR": str(root / "current")}):
                    self.assertEqual(state_dir(), root / "current")

    def test_status_reads_do_not_translate_or_write_old_owner_records(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "codex-swarm-status.test.json"
            rows = [{"name": "old", "threadId": "old", "boardOwner": "old-owner"},
                    {"name": "current", "threadId": "current", "agentOwner": "current-owner"}]
            path.write_text(json.dumps(rows))
            before = path.read_bytes()
            actual = read_threads(root)
            self.assertNotIn("agentOwner", actual[0])
            self.assertEqual(actual[1]["agentOwner"], "current-owner")
            self.assertEqual(path.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
