#!/usr/bin/env python3
"""The backend raises its soft open-file limit so native children inherit it."""
from pathlib import Path
import resource
import subprocess
import sys
import unittest

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
PROBE = (
    "import sys, resource, subprocess; sys.path.insert(0, %r); "
    "from codex_canvas import raise_open_file_limit; "
    "print(raise_open_file_limit()); "
    "print(subprocess.run([sys.executable, '-c', "
    "'import resource; print(resource.getrlimit(resource.RLIMIT_NOFILE)[0])'], "
    "capture_output=True, text=True, check=True).stdout.strip())"
) % str(SCRIPTS)


def run_with(soft, hard):
    def limit():
        resource.setrlimit(resource.RLIMIT_NOFILE, (soft, hard))
    result = subprocess.run([sys.executable, "-B", "-c", PROBE], preexec_fn=limit,
                            capture_output=True, text=True, check=True, timeout=60)
    return [int(line) for line in result.stdout.split()]


class OpenFileLimit(unittest.TestCase):
    def test_launchd_default_is_raised_and_inherited(self):
        hard = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
        own, child = run_with(256, hard)
        expected = 65536 if hard == resource.RLIM_INFINITY else min(65536, hard)
        self.assertEqual((own, child), (expected, expected))

    def test_low_hard_limit_is_respected(self):
        own, child = run_with(256, 1024)
        self.assertEqual((own, child), (1024, 1024))

    def test_higher_soft_limit_is_not_lowered(self):
        own, _ = run_with(100000, resource.RLIM_INFINITY)
        self.assertEqual(own, 100000)


if __name__ == "__main__":
    unittest.main(verbosity=2)
