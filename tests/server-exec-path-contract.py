#!/usr/bin/env python3
"""Absolute working directory syntax for local and remote Windows servers."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from codex_server_exec import validate


class ServerExecPathContract(unittest.TestCase):
    def payload(self, cwd):
        return validate({"cwd": cwd, "command": ["git", "status"], "timeout": 10,
                         "output_limit": 1024})

    def test_accepts_windows_drive_and_unc_absolute_paths(self):
        for cwd in (r"C:\Users\IGOS3\studio-dev", r"\\kukuka-win\studio-dev\repo"):
            with self.subTest(cwd=cwd):
                self.assertEqual(self.payload(cwd)["cwd"], cwd)

    def test_rejects_relative_and_drive_relative_paths(self):
        for cwd in ("studio-dev", r"C:Users\IGOS3", r"\Users\IGOS3"):
            with self.subTest(cwd=cwd), self.assertRaisesRegex(ValueError, "absolute cwd"):
                self.payload(cwd)


if __name__ == "__main__":
    unittest.main()
