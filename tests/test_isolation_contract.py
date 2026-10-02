"""Verify Python loads the shared environment scrub before contract startup."""
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest


class TestStartupIsolation(unittest.TestCase):
    def test_shared_fixture_removes_inherited_supervisor_routing(self):
        names = (
            "CODEX_AGENTS_SUPERVISOR_MODE",
            "CODEX_AGENTS_STATE_DIR",
            "CODEX_AGENTS_SUPERVISOR_FALLBACK",
        )
        env = os.environ.copy()
        env.update({name: "decoy" for name in names})
        helper = Path(__file__).resolve().with_name("test_isolation.py")
        code = (
            "import json, os, runpy; runpy.run_path(" + repr(str(helper)) + "); "
            "print(json.dumps({k: os.environ.get(k) for k in " + repr(names) + "}))"
        )
        proc = subprocess.run(
            [sys.executable, "-B", "-c", code],
            cwd=Path(__file__).resolve().parents[1],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual(json.loads(proc.stdout), {name: None for name in names})


if __name__ == "__main__":
    unittest.main()
