"""Reject escaped host services without touching another suite's state."""

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

RUNNER_SPEC = importlib.util.spec_from_file_location("host_exec_suite_runner", Path(__file__).with_name("run.py"))
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
RUNNER_SPEC.loader.exec_module(RUNNER)


@unittest.skipIf(os.name == "nt", "Windows children belong to the runner job")
class HostExecTeardown(unittest.TestCase):
    def test_detached_service_fails_owning_suite_and_is_stopped(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            scratch = root / "suite"
            scratch.mkdir()
            script = root / "codex_host_exec_server.py"
            script.write_text("import time; time.sleep(60)\n")
            child = subprocess.Popen([sys.executable, str(script), str(scratch / "state")],
                                     start_new_session=True)
            neighbor = subprocess.Popen([sys.executable, str(script), str(root / "suite-other" / "state")],
                                        start_new_session=True)
            try:
                error = RUNNER._check_host_exec_teardown(scratch)
                self.assertIn("host_exec children outlived suite teardown", error)
                self.assertIn(str(child.pid), error)
                self.assertNotEqual(child.wait(timeout=5), 0)
                self.assertIsNone(neighbor.poll())
                self.assertIsNone(RUNNER._check_host_exec_teardown(scratch))
            finally:
                for process in (child, neighbor):
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=5)

    def test_clean_teardown_passes(self):
        with tempfile.TemporaryDirectory() as scratch:
            self.assertIsNone(RUNNER._check_host_exec_teardown(scratch))


if __name__ == "__main__":
    unittest.main()
