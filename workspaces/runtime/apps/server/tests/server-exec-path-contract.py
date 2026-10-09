#!/usr/bin/env python3
"""Absolute working directory syntax for local and remote Windows servers."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from pathlib import Path
import os
import sys
import unittest

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))

from codex_server_exec import validate


class ServerExecPathContract(unittest.TestCase):
    def payload(self, cwd):
        return validate({"cwd": cwd, "command": ["git", "status"], "timeout": 10,
                         "output_limit": 1024})

    def remote_payload(self, cwd):
        return validate({"cwd": cwd, "command": ["git", "status"], "timeout": 10,
                         "output_limit": 1024}, allow_foreign_windows_path=True)

    def test_accepts_windows_drive_and_unc_absolute_paths(self):
        for cwd in (r"C:\Users\IGOS3\studio-dev", r"\\kukuka-win\studio-dev\repo"):
            with self.subTest(cwd=cwd):
                self.assertEqual(self.remote_payload(cwd)["cwd"], cwd)

    def test_target_requires_native_absolute_path_and_rejects_traversal(self):
        if os.name != "nt":
            for cwd in (r"C:\repo", r"\\host\share\repo"):
                with self.subTest(cwd=cwd), self.assertRaisesRegex(ValueError, "absolute cwd"):
                    self.payload(cwd)
        windows_paths = (r"C:\repo\..\outside", r"\\host\share\..\outside")
        for cwd in windows_paths:
            with self.subTest(cwd=cwd), self.assertRaisesRegex(ValueError, "traversal"):
                self.remote_payload(cwd)
            if os.name == "nt":
                with self.subTest(target_cwd=cwd), self.assertRaisesRegex(ValueError, "traversal"):
                    self.payload(cwd)
        if os.name != "nt":
            with self.assertRaisesRegex(ValueError, "traversal"):
                self.payload("/repo/../outside")

    def test_rejects_relative_and_drive_relative_paths(self):
        for cwd in ("studio-dev", r"C:Users\IGOS3", r"\Users\IGOS3"):
            with self.subTest(cwd=cwd), self.assertRaisesRegex(ValueError, "absolute cwd"):
                self.payload(cwd)


if __name__ == "__main__":
    unittest.main()
